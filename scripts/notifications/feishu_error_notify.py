#!/usr/bin/env python3
"""Read a safe turn-failure JSON payload from stdin and notify a Feishu bot."""
import argparse
import datetime
import ipaddress
import json
import os
import socket
import sys
import urllib.request


def private_ips():
    addresses = set()
    try:
        for item in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET):
            addresses.add(item[4][0])
    except OSError:
        pass
    # UDP connect selects the primary interface without sending a packet.
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.connect(("192.0.2.1", 80))
            addresses.add(sock.getsockname()[0])
    except OSError:
        pass
    networks = [ipaddress.ip_network(n) for n in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")]
    return sorted(a for a in addresses if any(ipaddress.ip_address(a) in n for n in networks))


def build_message(event):
    retryable = event.get("retryable")
    retry_label = "是" if retryable is True else "否" if retryable is False else "未知"
    fields = [
        "AionEasiful 会话错误通知",
        "时间: " + datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "hostname: " + socket.gethostname(),
        "内网 IP: " + (", ".join(private_ips()) or "未检测到 RFC1918 内网 IPv4"),
        "智能体: " + str(event.get("agent_name") or "未知"),
        "智能体 backend: " + str(event.get("agent_backend") or "未知"),
        "智能体 ID: " + str(event.get("agent_id") or "未提供"),
        "会话: " + str(event.get("conversation_id") or "未知"),
        "轮次: " + str(event.get("turn_id") or "未知"),
        "错误码: " + str(event.get("code") or "未分类"),
        "归属: " + str(event.get("ownership") or "未知"),
        "可重试: " + retry_label,
        "错误情况: " + str(event.get("situation") or "会话最终失败"),
    ]
    return {"msg_type": "text", "content": {"text": "\n".join(fields)}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test", action="store_true", help="Send a clearly marked test notification")
    parser.add_argument("--dry-run", action="store_true", help="Print payload without sending")
    args = parser.parse_args()
    event = ({"code": "NOTIFICATION_TEST", "situation": "通知链路测试，非真实会话错误"}
             if args.test else json.load(sys.stdin))
    message = build_message(event)
    if args.dry_run:
        print(json.dumps(message, ensure_ascii=False))
        return
    webhook = os.environ.get("AIONUI_FEISHU_ERROR_WEBHOOK", "")
    if not webhook.startswith("https://open.feishu.cn/open-apis/bot/v2/hook/"):
        raise ValueError("AIONUI_FEISHU_ERROR_WEBHOOK is missing or invalid")
    request = urllib.request.Request(webhook, data=json.dumps(message).encode(),
                                     headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(request, timeout=10) as response:
        result = json.load(response)
    if result.get("code", result.get("StatusCode", -1)) != 0:
        raise RuntimeError("Feishu bot rejected notification")
    print("Notification delivered")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Do not print exceptions containing the secret webhook URL.
        print("Notification failed: " + type(error).__name__, file=sys.stderr)
        sys.exit(1)
