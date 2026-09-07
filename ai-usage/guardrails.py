from pathlib import Path

root = Path(__file__).resolve().parents[1]
reader = (root / 'src/remote_status.rs').read_text(encoding='utf-8')
for forbidden in ('Command::new', 'std::process', 'ssh.exe', 'cmd.exe', 'auth.json', '.credentials.json'):
    assert forbidden not in reader, f'HTTP reader contains {forbidden}'
for required in ('TcpStream::connect_timeout', 'MAX_RESPONSE_BYTES', 'expires_at', 'ttl_seconds', 'supported_local_app_server', 'undocumented_fallback'):
    assert required in reader, f'HTTP reader missing {required}'
for provider in ('codex', 'claude'):
    source = (root / f'src/{provider}.rs').read_text(encoding='utf-8')
    assert 'return crate::remote_status::read_provider' in source
    assert source.count('return Ok(usage::UsageStatistics::default())') == 2
    assert 'activation is disabled in Home Linux display-only mode' in source
for name in ('src/provider.rs', 'src/popup_window/bridge.rs', 'src/store.rs', 'src/popup_window/cards.rs'):
    assert 'remote_status::handles_provider' in (root / name).read_text(encoding='utf-8'), name
updater = (root / 'src/updater.rs').read_text(encoding='utf-8')
assert 'in-place upstream updates are disabled' in updater
assert 'if crate::remote_status::enabled()' in updater.split('pub fn check_async', 1)[1].split('thread::spawn', 1)[0]
state = (root / 'src/popup_window/state.rs').read_text(encoding='utf-8')
assert 'Home Linux · UNKNOWN' in state
assert 'limits.primary.used_percent = None' in state
assert 'limits.secondary.used_percent = None' in state
print('PASS: HTTP-only reader, freshness, activation, hydration and pinned-update guardrails')
