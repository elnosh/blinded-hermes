use std::str::FromStr;

use crate::{
    invoice::{spawn_invoice_subscription, InvoiceState},
    models::{invoice::NewInvoice, zaps::NewZap},
    routes::{LnurlCallbackParams, LnurlCallbackResponse, LnurlVerifyResponse},
    State,
};
use anyhow::anyhow;
use fedimint_core::{config::FederationId, Amount, BitcoinHash};
use fedimint_ln_client::LightningClientModule;
use fedimint_ln_common::bitcoin::hashes::sha256;
use fedimint_ln_common::bitcoin::secp256k1::Parity;
use fedimint_ln_common::lightning_invoice::{Bolt11InvoiceDescription, Sha256};
use nostr::{Event, JsonUtil, Kind};

use crate::routes::{LnurlStatus, LnurlType, LnurlWellKnownResponse};

fn calc_metadata(name: &str, domain: &str) -> String {
    format!("[[\"text/identifier\",\"{name}@{domain}\"],[\"text/plain\",\"Sats for {name}\"]]")
}

pub async fn well_known_lnurlp(
    state: &State,
    name: String,
) -> anyhow::Result<LnurlWellKnownResponse> {
    let user = state.db.get_user_by_name(name.clone())?;
    if user.is_none() {
        return Err(anyhow!("NotFound"));
    }

    let res = LnurlWellKnownResponse {
        callback: format!("{}/lnurlp/{}/callback", state.domain, name).parse()?,
        max_sendable: Amount { msats: MAX_AMOUNT },
        min_sendable: Amount { msats: MIN_AMOUNT },
        metadata: calc_metadata(&name, &state.domain_no_http()),
        comment_allowed: None,
        tag: LnurlType::PayRequest,
        status: LnurlStatus::Ok,
        nostr_pubkey: Some(state.nostr.keys().await.public_key()),
        allows_nostr: true,
    };

    Ok(res)
}

const MAX_AMOUNT: u64 = 100_000_000 * 1_000; // 1 BTC
const MIN_AMOUNT: u64 = 1_000; // 1 sat

pub async fn lnurl_callback(
    state: &State,
    name: String,
    params: LnurlCallbackParams,
) -> anyhow::Result<LnurlCallbackResponse> {
    let user = state.db.get_user_and_increment_counter(&name)?;
    if user.is_none() {
        return Err(anyhow!("NotFound"));
    }
    let user = user.expect("just checked");

    if params.amount < MIN_AMOUNT {
        return Err(anyhow::anyhow!(
            "Amount ({}) < MIN_AMOUNT ({MIN_AMOUNT})",
            params.amount
        ));
    }

    if params.amount > MAX_AMOUNT {
        return Err(anyhow::anyhow!(
            "Amount ({}) < MAX_AMOUNT ({MAX_AMOUNT})",
            params.amount
        ));
    }

    // verify nostr param is a zap request
    if params
        .nostr
        .as_ref()
        .is_some_and(|n| Event::from_json(n).is_ok_and(|e| e.kind == Kind::ZapRequest))
    {
        return Err(anyhow::anyhow!("Invalid nostr event"));
    }

    let federation_id = FederationId::from_str(&user.federation_id)
        .map_err(|e| anyhow::anyhow!("Invalid federation_id: {e}"))?;

    let client = state
        .mm
        .get_federation_client(federation_id)
        .await
        .ok_or(anyhow!("NotFound"))?;

    let ln = client.get_first_module::<LightningClientModule>();

    // calculate description hash for invoice
    let desc_hash = match params.nostr {
        Some(ref nostr) => Sha256(sha256::Hash::hash(nostr.as_bytes())),
        None => {
            let metadata = calc_metadata(&name, &state.domain_no_http());
            Sha256(sha256::Hash::hash(metadata.as_bytes()))
        }
    };

    let invoice_index = user.invoice_index;

    let gateway = state
        .mm
        .get_gateway(&federation_id)
        .await
        .ok_or(anyhow!("Not gateway configured for federation"))?;

    let (op_id, pr, preimage) = ln
        .create_bolt11_invoice_for_user_tweaked(
            Amount::from_msats(params.amount),
            Bolt11InvoiceDescription::Hash(&desc_hash),
            Some(86_400), // 1 day expiry
            user.pubkey().public_key(Parity::Even), // todo is this parity correct / easy to work with?
            invoice_index as u64,
            (),
            Some(gateway),
        )
        .await?;

    // insert invoice into db for later verification
    let new_invoice = NewInvoice {
        federation_id: federation_id.to_string(),
        op_id: op_id.to_string(),
        preimage: hex::encode(preimage),
        app_user_id: user.id,
        user_invoice_index: invoice_index,
        bolt11: pr.to_string(),
        amount: params.amount as i64,
        state: InvoiceState::Pending as i32,
    };

    let created_invoice = state.db.insert_new_invoice(new_invoice)?;

    // save nostr zap request
    if let Some(request) = params.nostr {
        let new_zap = NewZap {
            request,
            event_id: None,
        };
        state.db.insert_new_zap(new_zap)?;
    }

    // create subscription to operation
    let subscription = ln
        .subscribe_ln_receive(op_id)
        .await
        .expect("subscribing to a just created operation can't fail");

    spawn_invoice_subscription(state.clone(), created_invoice, user.clone(), subscription).await;

    let verify_url = format!("{}/lnurlp/{}/verify/{}", state.domain, user.name, op_id);

    Ok(LnurlCallbackResponse {
        pr: pr.to_string(),
        success_action: None,
        status: LnurlStatus::Ok,
        reason: None,
        verify: verify_url.parse()?,
        routes: Some(vec![]),
    })
}

pub async fn verify(
    state: &State,
    name: String,
    op_id: String,
) -> anyhow::Result<LnurlVerifyResponse> {
    let invoice = state
        .db
        .get_invoice_by_op_id(op_id)?
        .ok_or(anyhow::anyhow!("NotFound"))?;

    let user = state
        .db
        .get_user_by_name(name)?
        .ok_or(anyhow::anyhow!("NotFound"))?;

    if invoice.app_user_id != user.id {
        return Err(anyhow::anyhow!("NotFound"));
    }

    let verify_response = LnurlVerifyResponse {
        status: LnurlStatus::Ok,
        settled: invoice.state == InvoiceState::Settled as i32,
        preimage: invoice.preimage,
        pr: invoice.bolt11,
    };

    Ok(verify_response)
}

#[cfg(all(test, feature = "integration-tests"))]
mod tests_integration {
    use nostr::{key::FromSkStr, Keys};
    use secp256k1::Secp256k1;
    use std::sync::Arc;

    use crate::{
        db::setup_db, lnurlp::well_known_lnurlp, mint::MockMultiMintWrapperTrait,
        models::app_user::NewAppUser, register::BlindSigner, State,
    };

    #[tokio::test]
    pub async fn well_known_nip5_lookup_test() {
        dotenv::dotenv().ok();
        let pg_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let db = setup_db(pg_url);

        // swap out fm with a mock here since that's not what is being tested
        let mock_mm = MockMultiMintWrapperTrait::new();

        // nostr
        let nostr_nsec_str = std::env::var("NSEC").expect("FM_DB_PATH must be set");
        let nostr_sk = Keys::from_sk_str(&nostr_nsec_str).expect("Invalid NOSTR_SK");
        let nostr = nostr_sdk::Client::new(&nostr_sk);

        // create blind signer
        let free_signer = BlindSigner::derive(&[0u8; 32], 0, 0);
        let paid_signer = BlindSigner::derive(&[0u8; 32], 0, 0);

        let mock_mm = Arc::new(mock_mm);
        let state = State {
            db: db.clone(),
            mm: mock_mm,
            secp: Secp256k1::new(),
            nostr,
            free_pk: free_signer.pk,
            paid_pk: paid_signer.pk,
            domain: "http://hello.com".to_string(),
        };

        let username = "wellknownuser".to_string();
        let user = NewAppUser {
            pubkey: "e6642fd69bd211f93f7f1f36ca51a26a5290eb2dd1b0d8279a87bb0d480c8443".to_string(),
            name: username.clone(),
            federation_id: "".to_string(),
            unblinded_msg: "".to_string(),
            federation_invite_code: "".to_string(),
        };

        // don't care about error if already exists
        let _ = state.db.insert_new_user(user);

        match well_known_lnurlp(&state, username.clone()).await {
            Ok(result) => {
                assert_eq!(
                    result.callback,
                    "http://hello.com/lnurlp/wellknownuser/callback"
                        .parse()
                        .unwrap()
                );
            }
            Err(e) => panic!("shouldn't error: {e}"),
        }
    }
}
