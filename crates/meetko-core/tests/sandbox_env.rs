//! `MEETKO_PLUGIN_SANDBOX=off` (own test binary: it sets a process-wide environment variable).
#![cfg(target_os = "macos")]

use std::time::Duration;

use meetko_core::host::Plugin;
use meetko_core::manifest::Manifest;

#[test]
fn env_off_is_an_explicit_opt_out_and_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("p.py"),
        "import sys, json\nfor line in sys.stdin:\n    m = json.loads(line)\n    print(json.dumps({'jsonrpc': '2.0', 'id': m['id'], 'result': {'contract': '0.1', 'plugin': {'id': 't/fake', 'version': '0'}, 'outputs': []}}), flush=True)\n",
    )
    .unwrap();
    let m = Manifest::parse(
        "[package]\nid=\"t/fake\"\nversion=\"0\"\nkind=\"plugin\"\ncontract=\"0.1\"\n[run]\ncommand=[\"python3\",\"-I\",\"-S\",\"p.py\"]\nprotocol=\"stdio-jsonrpc\"\n",
    )
    .unwrap();
    std::env::remove_var("MEETKO_PLUGIN_SANDBOX");
    assert_eq!(Plugin::spawn(&m, dir.path(), Duration::from_secs(10)).unwrap().sandbox, "macos-sbpl");
    std::env::set_var("MEETKO_PLUGIN_SANDBOX", "off");
    assert_eq!(Plugin::spawn(&m, dir.path(), Duration::from_secs(10)).unwrap().sandbox, "off");
    // anything but exactly "off" keeps the sandbox on
    std::env::set_var("MEETKO_PLUGIN_SANDBOX", "0");
    assert_eq!(Plugin::spawn(&m, dir.path(), Duration::from_secs(10)).unwrap().sandbox, "macos-sbpl");
}
