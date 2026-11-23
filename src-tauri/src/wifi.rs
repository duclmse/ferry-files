//! adb over Wi-Fi: pairing a phone (Android 11+, "Wireless debugging"), connecting to it, and
//! finding phones on the network. Once connected, a phone shows up in `adb devices` like a USB
//! one, with its address as the serial, and is used through `Adb` as usual.

use super::*;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// A phone with Wireless debugging open, found on the network (`adb mdns services`).
#[derive(Serialize, Debug, PartialEq)]
pub(super) struct Service {
    /// adb's name for the phone ("adb-R5CT…-AbCdEf"): the same for its pair and connect services.
    name: String,
    /// "pair": waiting for a pairing code; "connect": paired, ready to connect.
    kind: &'static str,
    /// "host:port".
    address: String,
}

/// Checks an address typed by hand: "host:port", nothing adb would take as an option.
pub(super) fn check_address(a: &str) -> Result<String, String> {
    let a = a.trim();
    let bad = || Err(format!("“{a}” isn't an address. Type it as shown on the phone, like 192.168.1.20:37000."));
    let Some((host, port)) = a.rsplit_once(':') else { return bad() };
    let host_ok = !host.is_empty() && !host.starts_with('-') && host.chars().all(|c| c.is_ascii_alphanumeric() || ".-_[]:".contains(c));
    if !host_ok || port.parse::<u16>().map_or(true, |p| p == 0) {
        return bad();
    }
    Ok(a.to_string())
}

/// Runs adb with `args`, giving up after `limit` (an address nobody answers can keep
/// `adb connect` waiting for a long time). Returns stdout and stderr together.
fn run(args: &[&str], limit: Duration) -> Result<String, String> {
    let Some(adb) = adb::adb_path() else {
        return Err(format!("adb isn't installed. {}", crate::platform::ADB_INSTALL));
    };
    let mut child = crate::platform::command(adb)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't run adb: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("The phone didn't answer. Check that it's on the same network and that Wireless debugging is on.".into());
            }
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("Couldn't run adb: {e}"))?;
    Ok(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// What `adb pair` said: Ok when paired, else its reason in plain words.
pub(super) fn pair_result(out: &str) -> Result<(), String> {
    if out.contains("Successfully paired") {
        return Ok(());
    }
    let why = out.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("adb said nothing.");
    if why.contains("Wrong password") || why.contains("wrong password") {
        return Err(
            "Pairing failed: the code was wrong, or it expired. Open “Pair device with pairing code” on the phone again and use the new code and port.".into(),
        );
    }
    Err(format!("Pairing failed: {why}"))
}

/// What `adb connect` said: Ok when connected (or already), else its reason. adb can report a
/// failure with exit status 0, so the words decide.
pub(super) fn connect_result(out: &str) -> Result<(), String> {
    let text = out.trim();
    if text.lines().any(|l| l.starts_with("connected to") || l.starts_with("already connected to")) {
        return Ok(());
    }
    if text.contains("authenticate") || text.contains("unauthorized") {
        return Err("The phone hasn't allowed this computer. Pair it first with a pairing code.".into());
    }
    if text.contains("refused") {
        return Err("The phone refused the connection. The port changes each time Wireless debugging is turned on: check the address on the phone.".into());
    }
    let why = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("adb said nothing.");
    Err(format!("Couldn't connect: {why}"))
}

/// Parses `adb mdns services`: a header line, then "name  service  host:port" per line.
pub(super) fn parse_services(out: &str) -> Vec<Service> {
    out.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let (name, service, address) = (w.next()?, w.next()?, w.next()?);
            let kind = if service.starts_with("_adb-tls-pairing.") {
                "pair"
            } else if service.starts_with("_adb-tls-connect.") {
                "connect"
            } else {
                return None;
            };
            check_address(address).ok().map(|address| Service { name: name.to_string(), kind, address })
        })
        .collect()
}

/// Pairs with a phone showing a pairing code ("Pair device with pairing code").
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn adb_pair(address: String, code: String) -> Result<(), String> {
    let address = check_address(&address)?;
    let code = code.trim().to_string();
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Type the pairing code shown on the phone (six digits).".into());
    }
    blocking(move || pair_result(&run(&["pair", &address, &code], Duration::from_secs(20))?)).await
}

/// Connects adb to a paired phone; it then shows up in `devices` with the address as serial.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn adb_connect(address: String) -> Result<String, String> {
    let address = check_address(&address)?;
    blocking(move || {
        connect_result(&run(&["connect", &address], Duration::from_secs(10))?)?;
        Ok(address)
    })
    .await
}

/// Phones on the network with Wireless debugging open. Empty when adb can't look (older adb, or
/// the network doesn't pass mDNS): addresses can still be typed.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn adb_discover() -> Result<Vec<Service>, String> {
    blocking(|| Ok(run(&["mdns", "services"], Duration::from_secs(5)).map(|o| parse_services(&o)).unwrap_or_default())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_checked() {
        assert_eq!(check_address(" 192.168.1.20:37000 ").unwrap(), "192.168.1.20:37000");
        assert!(check_address("pixel.local:5555").is_ok());
        assert!(check_address("192.168.1.20").is_err());
        assert!(check_address("192.168.1.20:0").is_err());
        assert!(check_address("192.168.1.20:99999").is_err());
        assert!(check_address("-H evil:5037").is_err());
        assert!(check_address("a b:1").is_err());
    }

    #[test]
    fn adb_answers_are_read() {
        assert!(pair_result("Successfully paired to 192.168.1.20:41235 [guid=adb-R5CT-AbCd]\n").is_ok());
        assert!(pair_result("Failed: Wrong password or connection was dropped.\n").unwrap_err().contains("code was wrong"));
        assert!(connect_result("connected to 192.168.1.20:37000\n").is_ok());
        assert!(connect_result("already connected to 192.168.1.20:37000\n").is_ok());
        assert!(connect_result("failed to connect to '192.168.1.20:37000': Connection refused\n").unwrap_err().contains("refused"));
        assert!(connect_result("failed to authenticate to 192.168.1.20:37000\n").unwrap_err().contains("Pair it first"));
    }

    #[test]
    fn mdns_services_are_parsed() {
        let out = "List of discovered mdns services\n\
            adb-R5CT10ABCDE-AbCdEf\t_adb-tls-connect._tcp\t192.168.1.20:37000\n\
            adb-R5CT10ABCDE-AbCdEf\t_adb-tls-pairing._tcp\t192.168.1.20:41235\n\
            other\t_something._tcp\t192.168.1.30:80\n";
        let s = parse_services(out);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], Service { name: "adb-R5CT10ABCDE-AbCdEf".into(), kind: "connect", address: "192.168.1.20:37000".into() });
        assert_eq!(s[1].kind, "pair");
    }
}
