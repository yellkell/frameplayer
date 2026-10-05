<?php
// yellkell.com/frameapps/unlock-api.php: the passthrough unlock API.
//
// Headset:  POST a=start               -> {code, token, page}
//           GET  a=claim&code&token    -> {state: waiting|unlocked|expired, licence?}
// Page:     GET  a=status&code         -> {state: unknown|waiting|unlocked, price}
//           POST a=pay     code        -> {url} (Stripe Checkout)
//           POST a=done    code session-> {state}
//           POST a=restore code email  -> {state} or {error}

declare(strict_types=1);

require dirname(__DIR__, 4) . '/frameplayer-unlock/lib.php';

$a = $_GET['a'] ?? '';
$in = fn(string $k) => trim((string) ($_POST[$k] ?? $_GET[$k] ?? ''));
$post = ($_SERVER['REQUEST_METHOD'] ?? '') === 'POST';
$price = '$' . number_format(FPU_PRICE_CENTS / 100, 2);

switch ($a) {
    case 'start':
        if (!$post) {
            fpu_json(['error' => 'POST'], 405);
        }
        $c = fpu_new_code();
        fpu_json($c + ['page' => 'yellkell.com/unlock', 'price' => $price]);

    case 'claim':
        $r = fpu_row('SELECT * FROM codes WHERE code = :c', [':c' => fpu_clean_code($in('code'))]);
        if (!$r || !hash_equals($r['token'], $in('token'))) {
            fpu_json(['error' => 'Unknown code'], 404);
        }
        $r = fpu_settle($r);
        if ($r['licence']) {
            fpu_json(['state' => 'unlocked', 'licence' => $r['licence']]);
        }
        fpu_json(['state' => $r['created'] < time() - FPU_CODE_TTL ? 'expired' : 'waiting']);

    case 'status':
        $r = fpu_code($in('code'));
        if (!$r) {
            fpu_json(['state' => 'unknown', 'price' => $price]);
        }
        $r = fpu_settle($r);
        fpu_json(['state' => $r['licence'] ? 'unlocked' : 'waiting', 'price' => $price]);

    case 'pay':
        if (!$post) {
            fpu_json(['error' => 'POST'], 405);
        }
        $r = fpu_code($in('code'));
        if (!$r) {
            fpu_json(['error' => "That code isn't right. Check it in FramePlayer."], 404);
        }
        if ($r['licence']) {
            fpu_json(['state' => 'unlocked']);
        }
        $s = fpu_checkout($r);
        if (isset($s['error']) || empty($s['url'])) {
            error_log('fpu checkout: ' . json_encode($s['error'] ?? $s));
            fpu_json(['error' => "Couldn't start the payment. Try again in a minute."], 502);
        }
        fpu_exec('UPDATE codes SET session = :s, checked = 0 WHERE code = :c',
            [':s' => $s['id'], ':c' => $r['code']]);
        fpu_json(['url' => $s['url']]);

    case 'done':
        if (!$post) {
            fpu_json(['error' => 'POST'], 405);
        }
        $r = fpu_code($in('code'));
        if (!$r) {
            fpu_json(['state' => 'unknown']);
        }
        if ($r['session'] && hash_equals($r['session'], $in('session'))) {
            $r = fpu_settle($r, true);
        }
        fpu_json(['state' => $r['licence'] ? 'unlocked' : 'waiting']);

    case 'restore':
        if (!$post) {
            fpu_json(['error' => 'POST'], 405);
        }
        $r = fpu_code($in('code'));
        if (!$r) {
            fpu_json(['error' => "That code isn't right. Check it in FramePlayer."], 404);
        }
        if ($r['licence']) {
            fpu_json(['state' => 'unlocked']);
        }
        $email = strtolower($in('email'));
        $p = $email === '' ? null : fpu_row(
            'SELECT * FROM purchases WHERE email = :e ORDER BY devices ASC, id DESC LIMIT 1',
            [':e' => $email]);
        if (!$p) {
            fpu_json(['error' => 'No purchase found for that email.'], 404);
        }
        if ($p['devices'] >= FPU_MAX_DEVICES) {
            fpu_json(['error' => 'That purchase is already on ' . FPU_MAX_DEVICES . ' headsets.'], 409);
        }
        fpu_grant($r, (int) $p['id']);
        fpu_json(['state' => 'unlocked']);

    default:
        fpu_json(['error' => 'Unknown action'], 400);
}
