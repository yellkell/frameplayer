//! Passthrough videos unlock: a one-time purchase at yellkell.com/unlock.
//!
//! FramePlayer asks the server for a short code; the buyer types it on a
//! phone or computer and pays with Stripe; the next claim brings back a
//! licence, kept in the settings and checked offline here against the
//! server's public key. Server: `tools/unlock-server`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const API: &str = "https://yellkell.com/frameapps/unlock-api.php";

/// What the unlock costs, for the button (the server has the real price).
pub const PRICE: &str = "$4.99";

/// Public half of the server's licence signing key: an uncompressed P-256
/// point. Licences are `<payload>.<hex DER ECDSA-SHA256 signature>`.
const PUBLIC_KEY: &str = "04ca31bc1db4f7b1bea7842ebc72d2f5788fad5273e1c41eeaf491c641b9191887\
                          112d85586aa8c454a755b21310a14e4842bd888ef34faefb2c3773d79d0dc455";

/// How long to wait for a payment before giving up the code.
const PATIENCE: Duration = Duration::from_secs(30 * 60);
const POLL: Duration = Duration::from_secs(3);

/// Whether `licence` is a passthrough licence signed by the server.
pub fn licence_valid(licence: &str) -> bool {
    let Some((payload, sig)) = licence.trim().rsplit_once('.') else {
        return false;
    };
    if !payload.starts_with("fpu1|passthrough|") {
        return false;
    }
    let (Some(key), Some(sig)) = (hex(PUBLIC_KEY), hex(sig)) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, key)
        .verify(payload.as_bytes(), &sig)
        .is_ok()
}

fn hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Where a purchase is.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Asking the server for a code.
    Starting,
    /// Show the code and the page to enter it on.
    Waiting {
        code: String,
        page: String,
        price: String,
    },
    /// Paid: the licence to keep.
    Unlocked(String),
    Failed(String),
}

/// A purchase in progress: a background thread gets a code, then polls the
/// server until it is paid for. Dropping it stops the thread.
pub struct Purchase {
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
}

impl Purchase {
    pub fn start() -> Purchase {
        let status = Arc::new(Mutex::new(Status::Starting));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, st) = (status.clone(), stop.clone());
        std::thread::Builder::new()
            .name("unlock".into())
            .spawn(move || {
                let end = run(&s, &st);
                *s.lock().unwrap() = end;
            })
            .expect("spawn unlock thread");
        Purchase { status, stop }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}

impl Drop for Purchase {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(serde::Deserialize)]
struct Started {
    code: String,
    token: String,
    page: String,
    #[serde(default)]
    price: String,
}

#[derive(serde::Deserialize)]
struct Claim {
    #[serde(default)]
    state: String,
    #[serde(default)]
    licence: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

const OFFLINE: &str = "Couldn't reach yellkell.com. Check the headset's Wi-Fi and try again.";

fn request(agent: &ureq::Agent, post: bool, url: &str) -> Result<(u16, String), String> {
    let r = if post {
        agent.post(url).send_empty()
    } else {
        agent.get(url).call()
    };
    let mut r = r.map_err(|e| {
        log::warn!("unlock: {e}");
        OFFLINE.to_string()
    })?;
    let status = r.status().as_u16();
    let body = r
        .body_mut()
        .read_to_string()
        .map_err(|_| OFFLINE.to_string())?;
    Ok((status, body))
}

fn run(status: &Mutex<Status>, stop: &AtomicBool) -> Status {
    let agent = fp_updater::default_agent();
    let started = match request(&agent, true, &format!("{API}?a=start")) {
        Ok((200, body)) => match serde_json::from_str::<Started>(&body) {
            Ok(s) => s,
            Err(_) => return Status::Failed(OFFLINE.into()),
        },
        Ok((_, body)) => {
            let msg = serde_json::from_str::<Claim>(&body)
                .ok()
                .and_then(|c| c.error);
            return Status::Failed(msg.unwrap_or_else(|| OFFLINE.into()));
        }
        Err(e) => return Status::Failed(e),
    };
    log::info!("unlock: waiting for code {}", started.code);
    *status.lock().unwrap() = Status::Waiting {
        code: started.code.clone(),
        page: started.page,
        price: started.price,
    };
    let url = format!(
        "{API}?a=claim&code={}&token={}",
        started.code, started.token
    );
    let since = Instant::now();
    while since.elapsed() < PATIENCE {
        let next = Instant::now() + POLL;
        while Instant::now() < next {
            if stop.load(Ordering::Relaxed) {
                return Status::Failed("Cancelled".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Network blips while the buyer pays are fine: try again next poll.
        let Ok((_, body)) = request(&agent, false, &url) else {
            continue;
        };
        let Ok(c) = serde_json::from_str::<Claim>(&body) else {
            continue;
        };
        match (c.state.as_str(), c.licence) {
            ("unlocked", Some(l)) if licence_valid(&l) => {
                log::info!("unlock: passthrough unlocked");
                return Status::Unlocked(l);
            }
            ("unlocked", _) => {
                log::warn!("unlock: the server sent a licence that doesn't verify");
                return Status::Failed(
                    "The unlock didn't check out. Update FramePlayer and try again.".into(),
                );
            }
            ("expired", _) => break,
            _ => {}
        }
    }
    Status::Failed("That code has expired. Get a new one to try again.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Signed by the server's key (tools/unlock-server, fpu_licence(0, "TEST00")).
    const SAMPLE: &str = "fpu1|passthrough|0|TEST00.3045022057496ab71c8c0cbfb91b802af4c37bf3ea1bba\
                          d64bf66f67437cd731267dc5a20221009b58d9cf5e8d7c9fe3bb53e5150b03db4a3781ba\
                          374213808c835884bb1b0464";

    #[test]
    fn server_licences_verify_and_tampered_ones_do_not() {
        assert!(licence_valid(SAMPLE));
        assert!(licence_valid(&format!(" {SAMPLE}\n")));
        assert!(!licence_valid(&SAMPLE.replace("TEST00", "TEST01")));
        assert!(!licence_valid(&SAMPLE.replace("passthrough", "everything")));
        assert!(!licence_valid(&SAMPLE[..SAMPLE.len() - 2]));
        assert!(!licence_valid("fpu1|passthrough|0|TEST00.zz"));
        assert!(!licence_valid(""));
    }
}
