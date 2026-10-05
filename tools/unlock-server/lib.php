<?php
// FramePlayer passthrough unlock: codes, Stripe Checkout and signed licences.
//
// Deployed outside the web root (~/frameplayer-unlock/lib.php) next to its
// state: unlock.sqlite, signing.pem (made on first use, never leaves the
// server) and stripe-key (pasted once through unlock-setup.php).
//
// The headset asks for a short code, the buyer types it on
// yellkell.com/unlock and pays, and the headset's next claim returns a
// licence: "fpu1|passthrough|<purchase>|<code>" signed with ECDSA P-256
// (SHA-256, DER), which FramePlayer checks offline against signing.pem's
// public key.

declare(strict_types=1);

const FPU_DIR = __DIR__;
const FPU_PRICE_CENTS = 499;
const FPU_CURRENCY = 'usd';
const FPU_PRODUCT = 'FramePlayer passthrough unlock';
const FPU_PAGE = 'https://yellkell.com/unlock';
// Headsets one purchase can restore onto.
const FPU_MAX_DEVICES = 5;
// Unpaid codes stop working after a day.
const FPU_CODE_TTL = 86400;
// No 0/O, 1/I/L: the code is read off a headset and typed on a phone.
const FPU_ALPHABET = 'ABCDEFGHJKMNPQRSTUVWXYZ23456789';

function fpu_db(): SQLite3
{
    static $db = null;
    if ($db) {
        return $db;
    }
    $db = new SQLite3(FPU_DIR . '/unlock.sqlite');
    $db->busyTimeout(5000);
    $db->exec('PRAGMA journal_mode=WAL');
    $db->exec('CREATE TABLE IF NOT EXISTS purchases (
        id INTEGER PRIMARY KEY,
        session TEXT UNIQUE NOT NULL,
        email TEXT,
        created INTEGER NOT NULL,
        devices INTEGER NOT NULL DEFAULT 0)');
    $db->exec('CREATE TABLE IF NOT EXISTS codes (
        code TEXT PRIMARY KEY,
        token TEXT NOT NULL,
        ip TEXT NOT NULL,
        created INTEGER NOT NULL,
        session TEXT,
        checked INTEGER NOT NULL DEFAULT 0,
        purchase INTEGER,
        licence TEXT)');
    return $db;
}

function fpu_row(string $sql, array $args = []): ?array
{
    $st = fpu_db()->prepare($sql);
    foreach ($args as $k => $v) {
        $st->bindValue($k, $v);
    }
    $r = $st->execute()->fetchArray(SQLITE3_ASSOC);
    return $r === false ? null : $r;
}

function fpu_exec(string $sql, array $args = []): void
{
    $st = fpu_db()->prepare($sql);
    foreach ($args as $k => $v) {
        $st->bindValue($k, $v);
    }
    $st->execute();
}

function fpu_json(array $body, int $status = 200): never
{
    http_response_code($status);
    header('Content-Type: application/json');
    header('Cache-Control: no-store');
    echo json_encode($body);
    exit;
}

function fpu_ip(): string
{
    // Stored hashed: only used to rate-limit new codes.
    return hash('sha256', ($_SERVER['REMOTE_ADDR'] ?? '') . '|fpu');
}

/// Normalises what the buyer typed: no spaces or dashes, upper case.
function fpu_clean_code(string $s): string
{
    return strtoupper(preg_replace('/[\s-]+/', '', $s));
}

function fpu_new_code(): array
{
    $n = (int) (fpu_row('SELECT COUNT(*) AS n FROM codes WHERE ip = :ip AND created > :t',
        [':ip' => fpu_ip(), ':t' => time() - 3600])['n'] ?? 0);
    if ($n >= 30) {
        fpu_json(['error' => 'Too many codes from here; try again in an hour.'], 429);
    }
    for ($try = 0; $try < 10; $try++) {
        $code = '';
        for ($i = 0; $i < 6; $i++) {
            $code .= FPU_ALPHABET[random_int(0, strlen(FPU_ALPHABET) - 1)];
        }
        if (!fpu_row('SELECT code FROM codes WHERE code = :c', [':c' => $code])) {
            $token = bin2hex(random_bytes(16));
            fpu_exec('INSERT INTO codes (code, token, ip, created) VALUES (:c, :t, :ip, :now)',
                [':c' => $code, ':t' => $token, ':ip' => fpu_ip(), ':now' => time()]);
            return ['code' => $code, 'token' => $token];
        }
    }
    fpu_json(['error' => 'Could not make a code'], 500);
}

/// A code the buyer may pay for or restore onto: known and not stale.
function fpu_code(string $code): ?array
{
    $r = fpu_row('SELECT * FROM codes WHERE code = :c', [':c' => fpu_clean_code($code)]);
    if (!$r || (!$r['licence'] && $r['created'] < time() - FPU_CODE_TTL)) {
        return null;
    }
    return $r;
}

// ---- Signing ----

function fpu_key()
{
    $path = FPU_DIR . '/signing.pem';
    if (!is_file($path)) {
        $k = openssl_pkey_new(['private_key_type' => OPENSSL_KEYTYPE_EC, 'curve_name' => 'prime256v1']);
        openssl_pkey_export($k, $pem);
        // Exclusive create: if two first requests race, the first key wins.
        $old = umask(0077);
        $f = @fopen($path, 'x');
        umask($old);
        if ($f) {
            fwrite($f, $pem);
            fclose($f);
        }
    }
    $key = openssl_pkey_get_private((string) @file_get_contents($path));
    if (!$key) {
        fpu_json(['error' => 'Signing key unavailable'], 500);
    }
    return $key;
}

/// The public key as an uncompressed P-256 point, hex (what FramePlayer embeds).
function fpu_public_hex(): string
{
    $d = openssl_pkey_get_details(fpu_key())['ec'];
    $pad = fn($b) => str_pad($b, 32, "\0", STR_PAD_LEFT);
    return bin2hex("\x04" . $pad($d['x']) . $pad($d['y']));
}

function fpu_licence(int $purchase, string $code): string
{
    $payload = "fpu1|passthrough|$purchase|$code";
    openssl_sign($payload, $sig, fpu_key(), OPENSSL_ALGO_SHA256);
    return $payload . '.' . bin2hex($sig);
}

/// Gives the code a licence for the purchase (once per code).
function fpu_grant(array $code, int $purchase): void
{
    if ($code['licence']) {
        return;
    }
    fpu_exec('UPDATE codes SET purchase = :p, licence = :l WHERE code = :c AND licence IS NULL',
        [':p' => $purchase, ':l' => fpu_licence($purchase, $code['code']), ':c' => $code['code']]);
    if (fpu_db()->changes() > 0) {
        fpu_exec('UPDATE purchases SET devices = devices + 1 WHERE id = :p', [':p' => $purchase]);
    }
}

// ---- Stripe ----

function fpu_stripe_key(): ?string
{
    $path = FPU_DIR . '/stripe-key';
    return is_file($path) ? trim(file_get_contents($path)) : null;
}

function fpu_stripe(string $method, string $path, array $form = [], ?string $key = null): array
{
    $key ??= fpu_stripe_key();
    if (!$key) {
        fpu_json(['error' => "Payments aren't switched on yet."], 503);
    }
    $ch = curl_init('https://api.stripe.com/v1/' . $path
        . ($method === 'GET' && $form ? '?' . http_build_query($form) : ''));
    curl_setopt_array($ch, [
        CURLOPT_RETURNTRANSFER => true,
        CURLOPT_USERPWD => $key . ':',
        CURLOPT_TIMEOUT => 20,
        CURLOPT_HTTPHEADER => ['Stripe-Version: 2024-06-20'],
    ]);
    if ($method === 'POST') {
        curl_setopt($ch, CURLOPT_POST, true);
        curl_setopt($ch, CURLOPT_POSTFIELDS, http_build_query($form));
    }
    $body = curl_exec($ch);
    $status = (int) curl_getinfo($ch, CURLINFO_HTTP_CODE);
    curl_close($ch);
    $json = is_string($body) ? json_decode($body, true) : null;
    if (!is_array($json)) {
        return ['error' => ['message' => 'Stripe did not answer']];
    }
    if ($status >= 400 && !isset($json['error'])) {
        $json['error'] = ['message' => "Stripe error $status"];
    }
    return $json;
}

function fpu_checkout(array $code): array
{
    $c = $code['code'];
    return fpu_stripe('POST', 'checkout/sessions', [
        'mode' => 'payment',
        'line_items' => [[
            'quantity' => 1,
            'price_data' => [
                'currency' => FPU_CURRENCY,
                'unit_amount' => FPU_PRICE_CENTS,
                'product_data' => [
                    'name' => FPU_PRODUCT,
                    'description' => 'Passthrough videos in FramePlayer for Steam Frame: '
                        . 'background removal and built-in masks. One-time unlock.',
                ],
            ],
        ]],
        'client_reference_id' => $c,
        'metadata' => ['product' => 'frameplayer-passthrough', 'code' => $c],
        'success_url' => FPU_PAGE . '?c=' . $c . '&session={CHECKOUT_SESSION_ID}',
        'cancel_url' => FPU_PAGE . '?c=' . $c,
    ]);
}

/// Checks the code's checkout with Stripe; on payment records the purchase
/// and grants the licence. Returns the code row as it is afterwards.
function fpu_settle(array $code, bool $force = false): array
{
    if ($code['licence'] || !$code['session']) {
        return $code;
    }
    // The headset polls every few seconds; ask Stripe at most every 4.
    if (!$force && $code['checked'] > time() - 4) {
        return $code;
    }
    fpu_exec('UPDATE codes SET checked = :t WHERE code = :c', [':t' => time(), ':c' => $code['code']]);
    $s = fpu_stripe('GET', 'checkout/sessions/' . rawurlencode($code['session']));
    $paid = ($s['payment_status'] ?? '') === 'paid'
        && ($s['client_reference_id'] ?? '') === $code['code'];
    if (!$paid) {
        return $code;
    }
    $email = strtolower(trim((string) ($s['customer_details']['email'] ?? '')));
    fpu_exec('INSERT OR IGNORE INTO purchases (session, email, created) VALUES (:s, :e, :t)',
        [':s' => $s['id'], ':e' => $email, ':t' => time()]);
    $p = fpu_row('SELECT id FROM purchases WHERE session = :s', [':s' => $s['id']]);
    fpu_grant($code, (int) $p['id']);
    return fpu_row('SELECT * FROM codes WHERE code = :c', [':c' => $code['code']]);
}
