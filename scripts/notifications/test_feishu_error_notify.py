import io
import json
import unittest
from unittest.mock import patch

import feishu_error_notify as notify


class NotificationTests(unittest.TestCase):
    def test_message_contains_device_and_failure_without_extra_payload(self):
        with patch.object(notify.socket, "gethostname", return_value="test-host"), \
                patch.object(notify, "private_ips", return_value=["192.168.1.8"]):
            payload = notify.build_message({"code": "UNKNOWN_UPSTREAM_ERROR", "situation": "安全拦截",
                                            "conversation_id": "conv", "turn_id": "turn",
                                            "agent_name": "Custom Codex", "agent_backend": "codex",
                                            "agent_id": "agent-123", "raw_response": "secret"})
        text = payload["content"]["text"]
        self.assertTrue(text.startswith("AionEasiful 会话错误通知\n"))
        for expected in ("test-host", "192.168.1.8", "UNKNOWN_UPSTREAM_ERROR", "安全拦截", "conv", "turn", "智能体: Custom Codex",
                         "智能体 backend: codex", "智能体 ID: agent-123"):
            self.assertIn(expected, text)
        self.assertNotIn("secret", text)

    def test_retryability_labels(self):
        for value, label in [(True, "是"), (False, "否"), (None, "未知")]:
            with patch.object(notify, "private_ips", return_value=[]):
                text = notify.build_message({"retryable": value})["content"]["text"]
            self.assertIn("可重试: " + label, text)
            self.assertIn("未检测到 RFC1918 内网 IPv4", text)

    def test_business_error_fails_even_with_http_success(self):
        with patch.object(notify.sys, "argv", ["notify", "--test"]), \
                patch.dict(notify.os.environ, {"AIONUI_FEISHU_ERROR_WEBHOOK": "https://open.feishu.cn/open-apis/bot/v2/hook/test"}), \
                patch.object(notify.urllib.request, "urlopen") as urlopen:
            urlopen.return_value.__enter__.return_value = io.StringIO(json.dumps({"code": 19024}))
            with self.assertRaises(RuntimeError):
                notify.main()

    def test_dry_run_does_not_contact_webhook(self):
        with patch.object(notify.sys, "argv", ["notify", "--test", "--dry-run"]), \
                patch.object(notify.urllib.request, "urlopen") as urlopen, \
                patch.object(notify.sys, "stdout", io.StringIO()):
            notify.main()
            urlopen.assert_not_called()


if __name__ == "__main__":
    unittest.main()
