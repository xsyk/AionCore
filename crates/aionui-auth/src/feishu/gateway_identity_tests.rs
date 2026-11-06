use super::*;
use aionui_api_types::{FeishuGatewayIdentity, FeishuGatewayIdentityClaims, FeishuGatewayIdentityExchangeResponse};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use sha2::{Digest, Sha256};

const NOW: i64 = 1_800_000_000_000;
const TX: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const NONCE: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

fn fixture() -> (GatewayIdentityTrust, SigningKey, FeishuGatewayIdentityClaims) {
    let key = SigningKey::from_bytes(&[7; 32]);
    let public = key.verifying_key().to_bytes();
    let trust = GatewayIdentityTrust {
        node_id: "node_qa".into(),
        app_id: "cli_qa".into(),
        tenant_key: "tenant_qa".into(),
        verification_key: URL_SAFE_NO_PAD.encode(public),
        key_id: URL_SAFE_NO_PAD.encode(Sha256::digest(public)),
    };
    let claims = FeishuGatewayIdentityClaims {
        iss: "aion-feishu-route".into(),
        aud: trust.node_id.clone(),
        sub: "feishu:union_owner".into(),
        tenant_key: trust.tenant_key.clone(),
        iat: NOW / 1000,
        exp: NOW / 1000 + 120,
        jti: TX.into(),
        nonce_hash: URL_SAFE_NO_PAD.encode(Sha256::digest(NONCE.as_bytes())),
        identity: FeishuGatewayIdentity {
            union_id: "union_owner".into(),
            open_id: "open_owner".into(),
            tenant_key: trust.tenant_key.clone(),
            name: "QA owner".into(),
            email: String::new(),
            avatar_url: String::new(),
        },
    };
    (trust, key, claims)
}

fn signed(
    key: &SigningKey,
    trust: &GatewayIdentityTrust,
    claims: &FeishuGatewayIdentityClaims,
    alg: &str,
    kid: &str,
) -> FeishuGatewayIdentityExchange {
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"alg":alg,"typ":"JWT","kid":kid})).unwrap());
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
    let unsigned = format!("{header}.{payload}");
    let assertion = format!(
        "{unsigned}.{}",
        URL_SAFE_NO_PAD.encode(key.sign(unsigned.as_bytes()).to_bytes())
    );
    let mut projection = claims.identity.clone();
    projection.union_id = "untrusted_outer_identity".into();
    projection.tenant_key = trust.tenant_key.clone();
    FeishuGatewayIdentityExchange {
        identity: projection,
        assertion,
        expires_at: (claims.exp) * 1000,
    }
}

#[test]
fn gateway_identity_valid_go_shape_uses_only_verified_signed_identity() {
    let (trust, key, claims) = fixture();
    let result = signed(&key, &trust, &claims, "EdDSA", &trust.key_id);
    // The Go handler wraps ExchangeResult in another identity field.
    let wire = serde_json::to_vec(&json!({"identity":result})).unwrap();
    let response: FeishuGatewayIdentityExchangeResponse = serde_json::from_slice(&wire).unwrap();
    let user = trust
        .verify_assertion(&response.identity, TX, NONCE, NOW)
        .expect("a valid pinned Go-format identity must authenticate");
    assert_eq!(user.union_id, "union_owner");
    assert_eq!(user.tenant_key, "tenant_qa");
}

#[test]
fn gateway_identity_rejects_wrong_signature_and_claim_context() {
    let (trust, key, claims) = fixture();
    let mut cases = Vec::new();
    let mut wrong = claims.clone();
    wrong.aud = "node_other".into();
    cases.push(wrong);
    let mut wrong = claims.clone();
    wrong.iss = "other".into();
    cases.push(wrong);
    let mut wrong = claims.clone();
    wrong.tenant_key = "other".into();
    cases.push(wrong);
    let mut wrong = claims.clone();
    wrong.identity.tenant_key = "other".into();
    cases.push(wrong);
    let mut wrong = claims.clone();
    wrong.sub = "feishu:other".into();
    cases.push(wrong);
    let mut wrong = claims.clone();
    wrong.jti = "other".into();
    cases.push(wrong);
    let mut wrong = claims.clone();
    wrong.nonce_hash = "other".into();
    cases.push(wrong);
    let mut wrong = claims.clone();
    wrong.exp = NOW / 1000;
    cases.push(wrong);
    let mut wrong = claims.clone();
    wrong.iat = NOW / 1000 + 60;
    cases.push(wrong);
    let mut wrong = claims.clone();
    wrong.exp = NOW / 1000 + 3600;
    cases.push(wrong);
    for claims in cases {
        let result = signed(&key, &trust, &claims, "EdDSA", &trust.key_id);
        assert_eq!(
            trust.verify_assertion(&result, TX, NONCE, NOW).unwrap_err().code(),
            "FEISHU_GATEWAY_ASSERTION_INVALID"
        );
    }
    for (alg, kid) in [
        ("none", trust.key_id.as_str()),
        ("HS256", trust.key_id.as_str()),
        ("EdDSA", "foreign-key"),
    ] {
        let result = signed(&key, &trust, &claims, alg, kid);
        assert_eq!(
            trust.verify_assertion(&result, TX, NONCE, NOW).unwrap_err().code(),
            "FEISHU_GATEWAY_ASSERTION_INVALID"
        );
    }
    let result = signed(
        &SigningKey::from_bytes(&[8; 32]),
        &trust,
        &claims,
        "EdDSA",
        &trust.key_id,
    );
    assert_eq!(
        trust.verify_assertion(&result, TX, NONCE, NOW).unwrap_err().code(),
        "FEISHU_GATEWAY_ASSERTION_INVALID"
    );
}

#[test]
fn gateway_identity_actual_go_http_fixture_verifies_oauth_and_h5() {
    let capture: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/feishu_gateway_identity.json")).unwrap();
    let trust = GatewayIdentityTrust {
        node_id: capture["node"]["id"].as_str().unwrap().into(),
        app_id: capture["app_id"].as_str().unwrap().into(),
        tenant_key: capture["tenant_key"].as_str().unwrap().into(),
        verification_key: capture["pin"]["verification_key"].as_str().unwrap().into(),
        key_id: capture["pin"]["key_id"].as_str().unwrap().into(),
    };
    trust.validate().unwrap();
    for kind in ["oauth", "h5"] {
        let case = &capture["cases"][kind];
        let begin: aionui_api_types::FeishuGatewayIdentityBeginResponse =
            serde_json::from_str(case["begin"]["raw_body"].as_str().unwrap()).unwrap();
        let response: FeishuGatewayIdentityExchangeResponse =
            serde_json::from_str(case["exchange"]["raw_body"].as_str().unwrap()).unwrap();
        assert_eq!(
            response.identity.expires_at % 1000,
            789,
            "real Go millisecond projection must remain compatible with signed seconds"
        );
        let user = trust
            .verify_assertion(
                &response.identity,
                &begin.transaction.transaction_id,
                case["nonce"].as_str().unwrap(),
                capture["now_ms"].as_i64().unwrap(),
            )
            .unwrap();
        assert_eq!(user.union_id, "union_contract_test_only");
        assert_eq!(user.email.as_deref(), Some("contract@example.invalid"));
        assert_eq!(case["wrong_nonce"]["http_status"], 410);
        assert_eq!(case["replay"]["http_status"], 410);
    }
}

#[test]
fn gateway_identity_trust_rejects_foreign_pin_and_noncanonical_key() {
    let (mut trust, _, _) = fixture();
    trust.key_id = "foreign".into();
    assert_eq!(trust.validate().unwrap_err().code(), "FEISHU_GATEWAY_TRUST_INVALID");
    let (mut trust, _, _) = fixture();
    trust.verification_key.push('=');
    assert_eq!(trust.validate().unwrap_err().code(), "FEISHU_GATEWAY_TRUST_INVALID");
}

#[test]
fn gateway_identity_unsigned_expiry_cannot_change_verified_claims_deadline() {
    let (trust, key, claims) = fixture();
    for expiry in [0, i64::MAX] {
        let mut result = signed(&key, &trust, &claims, "EdDSA", &trust.key_id);
        result.expires_at = expiry;
        assert_eq!(
            trust.verify_assertion(&result, TX, NONCE, NOW).unwrap().union_id,
            "union_owner"
        );
        assert_eq!(
            trust
                .verify_assertion(&result, TX, NONCE, (claims.exp) * 1000)
                .unwrap_err()
                .code(),
            "FEISHU_GATEWAY_ASSERTION_INVALID"
        );
    }
}

#[test]
fn gateway_identity_issued_at_clock_skew_has_an_exact_30_second_limit() {
    let (trust, key, mut claims) = fixture();
    claims.iat = NOW / 1000 + 30;
    claims.exp = claims.iat + 120;
    let result = signed(&key, &trust, &claims, "EdDSA", &trust.key_id);
    assert!(trust.verify_assertion(&result, TX, NONCE, NOW).is_ok());
    claims.iat += 1;
    claims.exp += 1;
    let result = signed(&key, &trust, &claims, "EdDSA", &trust.key_id);
    assert_eq!(
        trust.verify_assertion(&result, TX, NONCE, NOW).unwrap_err().code(),
        "FEISHU_GATEWAY_ASSERTION_INVALID"
    );
}
