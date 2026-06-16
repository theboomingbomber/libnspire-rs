// Hardware probe for a connected TI-Nspire CX II (VID 0x0451, PID 0xe022).
// Used to capture baseline behaviour and to validate stability changes.
//
// Usage:
//   cargo run --example cx2_probe            # info + list root
//   cargo run --example cx2_probe -- idle    # info, sleep 60s, info again
//   cargo run --example cx2_probe -- loop 30 # poll info every 1s for 30s

use std::convert::TryFrom;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusb::{GlobalContext, Hotplug, HotplugBuilder, UsbContext};

// Global lock mirroring n-link's USB_LOCK, used by the `stress` mode.
static STRESS_LOCK: Mutex<()> = Mutex::new(());

struct NullMon;
impl Hotplug<GlobalContext> for NullMon {
    fn device_arrived(&mut self, _: rusb::Device<GlobalContext>) {}
    fn device_left(&mut self, _: rusb::Device<GlobalContext>) {}
}

const VID: u16 = 0x0451;
const PID_CX2: u16 = 0xe022;
const PID: u16 = 0xe012;

fn open() -> libnspire::Handle<rusb::GlobalContext> {
    // Retry: a libusb_reset_device during init can briefly re-enumerate the
    // device, so the first open after another process exited may transiently
    // fail with NoDevice.
    let mut last = String::new();
    for attempt in 0..10 {
        let dev = rusb::open_device_with_vid_pid(VID, PID_CX2)
            .or_else(|| rusb::open_device_with_vid_pid(VID, PID));
        match dev {
            Some(dev) => match libnspire::Handle::new(dev) {
                Ok(h) => return h,
                Err(e) => last = format!("{e:?}"),
            },
            None => last = "no device".into(),
        }
        std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
    }
    panic!("failed to open TI-Nspire after retries: {}", last);
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "info".to_string());
    match mode.as_str() {
        "info" => {
            let handle = open();
            match handle.info() {
                Ok(info) => println!("INFO OK: {}", serde_json::to_string(&info).unwrap()),
                Err(e) => println!("INFO ERR: {:?}", e),
            }
            match handle.list_dir("/") {
                Ok(dir) => {
                    println!("LIST OK: {} entries", dir.iter().count());
                    for f in dir.iter() {
                        println!("  {}", f.name().to_string_lossy());
                    }
                }
                Err(e) => println!("LIST ERR: {:?}", e),
            }
        }
        "idle" => {
            let secs: u64 = std::env::args()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(60);
            let handle = open();
            println!("before idle: {:?}", handle.info().map(|i| i.name));
            println!("sleeping {secs}s (leave calc untouched)...");
            std::thread::sleep(Duration::from_secs(secs));
            let start = Instant::now();
            let r = handle.info();
            println!(
                "after {secs}s idle ({}ms): {:?}",
                start.elapsed().as_millis(),
                r.map(|i| i.name).map_err(|e| format!("{e:?}"))
            );
        }
        "loop" => {
            let secs: u64 = std::env::args()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(30);
            let handle = open();
            let start = Instant::now();
            let mut n = 0u64;
            while start.elapsed() < Duration::from_secs(secs) {
                n += 1;
                match handle.info() {
                    Ok(_) => print!("."),
                    Err(e) => print!("[{n}:{e:?}]"),
                }
                use std::io::Write;
                std::io::stdout().flush().ok();
                std::thread::sleep(Duration::from_secs(1));
            }
            println!("\ndone: {n} polls over {secs}s");
        }
        "pull" => {
            // pull <calc-path> <size-bytes> [out-file]
            let path = std::env::args().nth(2).expect("usage: pull <calc-path> <size> [out]");
            let size: usize = std::env::args()
                .nth(3)
                .and_then(|s| s.parse().ok())
                .expect("need size in bytes (from `info`/ls)");
            let out = std::env::args().nth(4);
            let handle = open();
            let mut buf = vec![0u8; size];
            let start = Instant::now();
            let r = handle.read_file(&path, &mut buf, &mut |rem| {
                if rem % 65536 < 4096 {
                    print!("\r  {} / {} bytes", size - rem, size);
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
            });
            println!();
            match r {
                Ok(n) => {
                    let cs = crc32(&buf[..n.min(buf.len())]);
                    println!(
                        "PULL OK: {n} bytes in {}ms crc32={cs:08x}",
                        start.elapsed().as_millis()
                    );
                    if let Some(out) = out {
                        std::fs::write(&out, &buf[..n.min(buf.len())]).unwrap();
                        println!("wrote {out}");
                    }
                }
                Err(e) => println!("PULL ERR after {}ms: {e:?}", start.elapsed().as_millis()),
            }
        }
        "push" => {
            // push <local-file> <calc-dest-dir>
            let local = std::env::args().nth(2).expect("usage: push <local-file> <calc-dest-dir>");
            let dest = std::env::args().nth(3).expect("usage: push <local-file> <calc-dest-dir>");
            let buf = std::fs::read(&local).expect("read local file");
            let name = std::path::Path::new(&local)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let full = format!("{}/{}", dest.trim_end_matches('/'), name);
            let handle = open();
            let start = Instant::now();
            let r = handle.write_file(&full, &buf, &mut |rem| {
                if rem % 65536 < 4096 {
                    print!("\r  {} / {} bytes", buf.len() - rem, buf.len());
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
            });
            println!();
            match r {
                Ok(()) => println!(
                    "PUSH OK: {} bytes ({}) in {}ms crc32={:08x}",
                    buf.len(),
                    full,
                    start.elapsed().as_millis(),
                    crc32(&buf)
                ),
                Err(e) => println!("PUSH ERR after {}ms: {e:?}", start.elapsed().as_millis()),
            }
        }
        "stress" => {
            // Reproduce n-link's hotplug-pump-vs-sync-transfer conflict.
            //   cargo run --example cx2_probe -- stress         (no lock: should hang)
            //   cargo run --example cx2_probe -- stress lock     (serialized: should not hang)
            let use_lock = std::env::args().nth(2).as_deref() == Some("lock");
            println!("stress mode, use_lock={use_lock}");
            if rusb::has_hotplug() {
                let reg = HotplugBuilder::new()
                    .vendor_id(VID)
                    .register(GlobalContext::default(), Box::new(NullMon))
                    .unwrap();
                std::mem::forget(reg);
                std::thread::spawn(move || loop {
                    if use_lock {
                        {
                            let _g = STRESS_LOCK.lock().unwrap();
                            let _ = GlobalContext::default()
                                .handle_events(Some(Duration::from_millis(50)));
                        }
                        std::thread::sleep(Duration::from_millis(150));
                    } else {
                        let _ = GlobalContext::default().handle_events(None);
                    }
                });
            } else {
                println!("no hotplug support");
            }
            let handle = open();
            let start = Instant::now();
            let mut n = 0u32;
            while start.elapsed() < Duration::from_secs(60) {
                n += 1;
                let t = Instant::now();
                let r = if use_lock {
                    let _g = STRESS_LOCK.lock().unwrap();
                    handle.list_dir("/")
                } else {
                    handle.list_dir("/")
                };
                println!(
                    "op {n}: {} in {}ms",
                    r.map(|d| format!("{} entries", d.iter().count()))
                        .unwrap_or_else(|e| format!("{e:?}")),
                    t.elapsed().as_millis()
                );
                std::thread::sleep(Duration::from_millis(500));
            }
            println!("DONE: {n} ops over 60s with no hang");
        }
        "thrash" => {
            // Mimic the GUI under rapid clicking: hotplug event pump (serialized
            // by a lock, like n-link) + a fast mix of list_dir/info with no
            // pauses, to try to reproduce the protocol desync.
            let mode = std::env::args().nth(2).unwrap_or_default();
            let use_lock = mode != "nolock";
            let use_pump = mode != "nopump";
            let secs: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(60);
            println!("thrash use_lock={use_lock} use_pump={use_pump} for {secs}s");
            // Open FIRST (one reset), then start the pump, so the pump/reset race
            // on open doesn't muddy the browsing-desync test.
            let handle = open();
            let mut dirs = vec!["/".to_string()];
            if let Ok(d) = handle.list_dir("/") {
                for f in d.iter().take(6) {
                    if f.entry_type() == libnspire::dir::EntryType::Directory {
                        dirs.push(format!("/{}", f.name().to_string_lossy()));
                    }
                }
            }
            if use_pump && rusb::has_hotplug() {
                let reg = HotplugBuilder::new()
                    .vendor_id(VID)
                    .register(GlobalContext::default(), Box::new(NullMon))
                    .unwrap();
                std::mem::forget(reg);
                std::thread::spawn(move || loop {
                    if use_lock {
                        {
                            let _g = STRESS_LOCK.lock().unwrap();
                            let _ = GlobalContext::default()
                                .handle_events(Some(Duration::from_millis(50)));
                        }
                        std::thread::sleep(Duration::from_millis(150));
                    } else {
                        let _ = GlobalContext::default().handle_events(None);
                    }
                });
            }
            println!("navigating among: {dirs:?}");
            // Ensure a scratch dir exists for the mutation cycle.
            { let _g = STRESS_LOCK.lock().unwrap(); let _ = handle.create_dir("/nlthrash"); }
            let blob = vec![0x5Au8; 8000];
            let start = Instant::now();
            let (mut n, mut errs, mut first_err_at) = (0u64, 0u64, 0u128);
            while start.elapsed() < Duration::from_secs(secs) {
                n += 1;
                let r: Result<String, libnspire::Error> = {
                    let _g = if use_lock { Some(STRESS_LOCK.lock().unwrap()) } else { None };
                    match n % 6 {
                        0 => handle.info().map(|_| "info".into()),
                        // mutation sequence like upload+auto-refresh / delete
                        2 => handle.write_file("/nlthrash/t.tns", &blob, &mut |_| {}).map(|_| "write".into()),
                        4 => handle.delete_file("/nlthrash/t.tns").map(|_| "delete".into()),
                        _ => {
                            let p = &dirs[(n as usize) % dirs.len()];
                            handle.list_dir(p).map(|d| format!("ls {p} ({} entries)", d.iter().count()))
                        }
                    }
                };
                if let Err(e) = r {
                    errs += 1;
                    if first_err_at == 0 { first_err_at = start.elapsed().as_millis(); }
                    if errs <= 20 {
                        println!("op {n} @ {}ms ERR: {e:?}", start.elapsed().as_millis());
                    }
                }
                // no sleep: thrash
            }
            { let _g = STRESS_LOCK.lock().unwrap(); let _ = handle.delete_file("/nlthrash/t.tns"); let _ = handle.delete_dir("/nlthrash"); }
            println!("\nDONE: {n} ops, {errs} errors over {secs}s (first err @ {first_err_at}ms)");
        }
        "folder" => {
            use std::path::Path;
            // Mirror n-link's upload_dir/download_dir recursion to validate it.
            fn up<C: UsbContext>(h: &libnspire::Handle<C>, local: &Path, calc: &str) {
                match h.create_dir(calc) {
                    Ok(()) | Err(libnspire::Error::Exists) => {}
                    Err(e) => {
                        println!("  mkdir {calc} ERR {e:?}");
                        return;
                    }
                }
                for entry in std::fs::read_dir(local).unwrap() {
                    let entry = entry.unwrap();
                    let name = entry.file_name().to_string_lossy().to_string();
                    let child = format!("{}/{}", calc, name);
                    if entry.file_type().unwrap().is_dir() {
                        up(h, &entry.path(), &child);
                    } else {
                        let buf = std::fs::read(entry.path()).unwrap();
                        match h.write_file(&child, &buf, &mut |_| {}) {
                            Ok(()) => println!("  up {child}: {} bytes", buf.len()),
                            Err(e) => println!("  up {child} ERR {e:?}"),
                        }
                    }
                }
            }
            fn down<C: UsbContext>(h: &libnspire::Handle<C>, calc: &str, local: &Path) {
                std::fs::create_dir_all(local).unwrap();
                let dir = match h.list_dir(calc) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("  list {calc} ERR {e:?}");
                        return;
                    }
                };
                let entries: Vec<(String, bool, u64)> = dir
                    .iter()
                    .map(|f| {
                        (
                            f.name().to_string_lossy().to_string(),
                            f.entry_type() == libnspire::dir::EntryType::Directory,
                            f.size(),
                        )
                    })
                    .collect();
                drop(dir);
                for (name, is_dir, size) in entries {
                    let child = format!("{}/{}", calc, name);
                    let child_local = local.join(&name);
                    if is_dir {
                        down(h, &child, &child_local);
                    } else {
                        let mut buf = vec![0u8; size as usize];
                        match h.read_file(&child, &mut buf, &mut |_| {}) {
                            Ok(n) => {
                                std::fs::write(&child_local, &buf[..n.min(buf.len())]).unwrap();
                                println!("  down {child}: {n} bytes");
                            }
                            Err(e) => println!("  down {child} ERR {e:?}"),
                        }
                    }
                }
            }
            let root = Path::new("/tmp/nlfolder_test");
            std::fs::remove_dir_all(root).ok();
            std::fs::create_dir_all(root.join("sub")).unwrap();
            std::fs::write(root.join("a.tns"), b"hello-folder-a").unwrap();
            std::fs::write(root.join("sub").join("b.tns"), vec![0x42u8; 4096]).unwrap();
            let handle = open();
            println!("=== upload local tree -> /nlfolder_test ===");
            up(&handle, root, "/nlfolder_test");
            println!("=== download /nlfolder_test -> /tmp/nlfolder_back ===");
            let back = Path::new("/tmp/nlfolder_back");
            std::fs::remove_dir_all(back).ok();
            down(&handle, "/nlfolder_test", back);
            let cmp = |p: &str| std::fs::read(root.join(p)).ok() == std::fs::read(back.join(p)).ok();
            println!("a.tns match:     {}", cmp("a.tns"));
            println!("sub/b.tns match: {}", cmp("sub/b.tns"));
            // cleanup calc
            handle.delete_file("/nlfolder_test/a.tns").ok();
            handle.delete_file("/nlfolder_test/sub/b.tns").ok();
            handle.delete_dir("/nlfolder_test/sub").ok();
            handle.delete_dir("/nlfolder_test").ok();
            println!("cleaned up /nlfolder_test");
        }
        "enum" => {
            // Replicate n-link's add_device() exactly to see where GUI
            // enumeration drops the device.
            for dev in rusb::devices().unwrap().iter() {
                let d = match dev.device_descriptor() {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                if d.vendor_id() != VID || !matches!(d.product_id(), PID_CX2 | PID) {
                    continue;
                }
                println!(
                    "found {:04x}:{:04x} bus {} addr {}",
                    d.vendor_id(),
                    d.product_id(),
                    dev.bus_number(),
                    dev.address()
                );
                match dev.open() {
                    Ok(h) => {
                        println!("  open: OK");
                        match h.read_languages(Duration::from_millis(100)) {
                            Ok(langs) => {
                                println!("  languages: {:?}", langs);
                                match langs.first() {
                                    Some(l) => match h.read_product_string(
                                        *l,
                                        &d,
                                        Duration::from_millis(100),
                                    ) {
                                        Ok(s) => println!("  product_string: {s:?}"),
                                        Err(e) => println!("  read_product_string ERR: {e:?}"),
                                    },
                                    None => println!("  NO LANGUAGES -> langs[0] would PANIC"),
                                }
                            }
                            Err(e) => println!("  read_languages ERR: {e:?}"),
                        }
                    }
                    Err(e) => println!("  open ERR: {e:?}"),
                }
            }
        }
        "shot" => {
            let out = std::env::args()
                .nth(2)
                .unwrap_or_else(|| "/tmp/cx2_shot.png".to_string());
            let handle = open();
            match handle.screenshot() {
                Ok(img) => {
                    println!("SCREENSHOT OK: {}x{} bpp={}", img.width, img.height, img.bpp);
                    match image::DynamicImage::try_from(img) {
                        Ok(di) => {
                            di.save(&out).unwrap();
                            println!("saved {out}");
                        }
                        Err(e) => println!("convert err: {e:?}"),
                    }
                }
                Err(e) => println!("SCREENSHOT ERR: {e:?}"),
            }
        }
        "mkdir" => {
            let path = std::env::args().nth(2).expect("usage: mkdir <calc-path>");
            let handle = open();
            println!("MKDIR {path}: {:?}", handle.create_dir(&path));
        }
        "rm" => {
            let path = std::env::args().nth(2).expect("usage: rm <calc-path>");
            let handle = open();
            println!("RM {path}: {:?}", handle.delete_file(&path));
        }
        "rmdir" => {
            let path = std::env::args().nth(2).expect("usage: rmdir <calc-path>");
            let handle = open();
            println!("RMDIR {path}: {:?}", handle.delete_dir(&path));
        }
        other => println!("unknown mode: {other}"),
    }
}

/// Tiny dependency-free CRC32 (IEEE) for integrity checks.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
