<?php
// yellkell.com/frameapps/unlock-setup.php?t=<token>: paste the Stripe key once.
//
// Works only while ~/frameplayer-unlock/setup-token exists and matches ?t=;
// a key that Stripe accepts is saved to ~/frameplayer-unlock/stripe-key
// (never shown again) and the token is deleted, so the page closes itself.

declare(strict_types=1);

require dirname(__DIR__, 4) . '/frameplayer-unlock/lib.php';

header('Cache-Control: no-store');
header('X-Robots-Tag: noindex');
$tokenFile = FPU_DIR . '/setup-token';
$ok = is_file($tokenFile)
    && hash_equals(trim(file_get_contents($tokenFile)), (string) ($_GET['t'] ?? ''));
$msg = '';
$done = false;

if ($ok && ($_SERVER['REQUEST_METHOD'] ?? '') === 'POST') {
    $key = trim((string) ($_POST['key'] ?? ''));
    if (!preg_match('/^(rk|sk)_(live|test)_[A-Za-z0-9]{20,}$/', $key)) {
        $msg = 'That doesn\'t look like a Stripe secret or restricted key (rk_live_… or sk_live_…).';
    } else {
        $test = fpu_stripe('GET', 'checkout/sessions', ['limit' => 1], $key);
        if (isset($test['error'])) {
            $msg = 'Stripe refused that key: ' . htmlspecialchars((string) $test['error']['message'])
                . ' (a restricted key needs Checkout Sessions: Write).';
        } else {
            $path = FPU_DIR . '/stripe-key';
            file_put_contents($path, $key . "\n");
            chmod($path, 0600);
            unlink($tokenFile);
            fpu_key();
            $done = true;
        }
    }
}
?><!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>Unlock setup</title>
<style>
:root{--bg:#06070d;--card:#0f1019;--line:rgba(255,255,255,.08);--text:#f2f3f8;--dim:#8b8fa3;--blue:#0071e3}
*{box-sizing:border-box}
body{margin:0;min-height:100svh;display:grid;place-items:center;padding:20px;background:var(--bg);color:var(--text);font:16px/1.5 -apple-system,BlinkMacSystemFont,Inter,"Helvetica Neue",Arial,sans-serif}
main{width:min(520px,100%);padding:32px;border-radius:20px;background:var(--card);box-shadow:inset 0 0 0 1px var(--line)}
h1{margin:0 0 8px;font-size:24px}
p{color:var(--dim);margin:0 0 16px}
input{width:100%;padding:12px 14px;border-radius:12px;border:1px solid var(--line);background:#000;color:var(--text);font:inherit}
button{margin-top:14px;padding:12px 26px;border:0;border-radius:980px;background:var(--blue);color:#fff;font:500 16px/1 inherit;cursor:pointer}
.err{color:#ff8a8a}
</style>
</head>
<body>
<main>
<?php if ($done): ?>
  <h1>Payments are on</h1>
  <p>FramePlayer's passthrough unlock now takes payments. This page has closed itself.</p>
<?php elseif (!$ok): ?>
  <h1>Closed</h1>
  <p>This setup link has been used or isn't valid.</p>
<?php else: ?>
  <h1>Stripe key for the passthrough unlock</h1>
  <p>Paste a restricted key from Stripe (Developers → API keys → Create restricted key, with
     <b>Checkout Sessions: Write</b>). It's checked with Stripe, stored on this server only,
     and never shown again.</p>
  <?php if ($msg): ?><p class="err"><?= $msg ?></p><?php endif; ?>
  <form method="post" autocomplete="off">
    <input name="key" type="password" placeholder="rk_live_…" required spellcheck="false">
    <button>Save</button>
  </form>
<?php endif; ?>
</main>
</body>
</html>
