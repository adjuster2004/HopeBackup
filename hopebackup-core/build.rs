//! Время сборки (UTC) для `hopebackup --version`; заглушка панели, если она ещё не собрана.
fn main() {
    let secs = std::env::var("SOURCE_DATE_EPOCH").ok().and_then(|v| v.parse::<i64>().ok()).unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    });
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Дата по дням от эпохи (алгоритм Хиннанта).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    println!("cargo:rustc-env=HOPEBACKUP_BUILD_TIME={:04}-{:02}-{:02} {:02}:{:02}Z", y, m, d, rem / 3600, rem % 3600 / 60);
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../admin/dist");
    // Панель встраивается в бинарник из ../admin/dist; если её ещё не собирали — заглушка.
    let dist = std::path::Path::new("../admin/dist");
    if !dist.join("index.html").exists() {
        let _ = std::fs::create_dir_all(dist);
        let _ = std::fs::write(dist.join("index.html"), "<!doctype html><meta charset=utf-8><title>HopeBackup</title><p>Панель не собрана: cd admin &amp;&amp; npm ci &amp;&amp; npm run build</p>");
    }
}
