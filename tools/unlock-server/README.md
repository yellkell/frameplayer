# Passthrough unlock server

A one-time unlock of passthrough videos in FramePlayer (`crates/frameplayer/src/unlock.rs`), sold at yellkell.com/unlock through Stripe Checkout. It runs as plain PHP on the yellkell.com host, with no webhook: the server asks Stripe about each checkout itself.

1. FramePlayer `POST a=start` gets a six-character code and a secret token, and shows the code.
2. The buyer enters the code at yellkell.com/unlock (`public_html/unlock.html`) and pays. `a=pay` creates the Checkout Session, and Stripe returns them to the page, which confirms with `a=done`.
3. FramePlayer polls `a=claim` every 3 s. Once Stripe reports the session paid, the claim returns a licence, `fpu1|passthrough|<purchase>|<code>.<hex DER ECDSA P-256 SHA-256>`. FramePlayer checks it offline against the public key in `unlock.rs`.

Restore: on the page, the purchase email attaches the purchase to a new code, on up to 5 headsets.

## Files on the host

| Repo | Host |
|---|---|
| `lib.php` | `~/frameplayer-unlock/lib.php` (outside the web root) |
| `api.php` | `public_html/frameapps/unlock-api.php` |
| `setup.php` | `public_html/frameapps/unlock-setup.php` |
| (site repo) `unlock.html` | `public_html/unlock.html` |

`~/frameplayer-unlock/` also holds the following. None of these are in git.

- `unlock.sqlite`: codes and purchases.
- `signing.pem`: the licence key, made on first use. Back it up: if it is lost, licences already issued still verify, but new ones need a FramePlayer update with a new key.
- `stripe-key`: a restricted key with Checkout Sessions: Write. It is pasted once at `unlock-setup.php?t=<token>`, which works only while `~/frameplayer-unlock/setup-token` holds that token, then deletes it.

The price is `FPU_PRICE_CENTS` in `lib.php`. The `PRICE` in `unlock.rs` is only the button label.
