use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::usb::{self, Usb};

const REMOTE_AGENT: &str = "/userdata/.pencast/agent.sh";
const AGENT: &str = r#"set -eu

ROOT=/userdata/.pencast
GADGET=/sys/kernel/config/usb_gadget/rockchip
CONFIG=$GADGET/configs/b.1
FUNCTION=ffs.pencast
FUNCTION_PATH=$GADGET/functions/$FUNCTION
MOUNT=/dev/usb-ffs/pencast
WORKER=$ROOT/pencast-work
PID=$ROOT/worker.pid
UDC=$ROOT/original-udc
PRODUCT=$ROOT/original-product
LINK=$CONFIG/f3

mounted() {
    grep -qs " $MOUNT " /proc/mounts
}

linked() {
    [ -L "$LINK" ] || return 1
    case "$(readlink "$LINK" 2>/dev/null || :)" in
        "functions/$FUNCTION"|*/"functions/$FUNCTION") return 0 ;;
    esac
    return 1
}

stop_worker() {
    [ -f "$PID" ] || return 0
    worker=$(cat "$PID")
    rm -f "$PID"
    kill "$worker" 2>/dev/null || true
    attempt=0
    while kill -0 "$worker" 2>/dev/null && [ "$attempt" -lt 10 ]; do
        kill -KILL "$worker" 2>/dev/null || true
        sleep 0.1
        attempt=$((attempt + 1))
    done
}

restore() {
    [ -f "$UDC" ] || return 0
    [ -f "$PRODUCT" ]
    old_udc=$(cat "$UDC")
    old_product=$(cat "$PRODUCT")
    echo "" > "$GADGET/UDC" || true
    sleep 0.1
    linked && rm "$LINK" || true
    echo "$old_product" > "$GADGET/idProduct"
    [ -n "$old_udc" ] && echo "$old_udc" > "$GADGET/UDC"
}

discard() {
    stop_worker
    mounted && umount "$MOUNT" 2>/dev/null || true
    rmdir "$FUNCTION_PATH" 2>/dev/null || true
    rm -rf "$ROOT"
}

rollback() {
    status=$?
    trap - EXIT HUP INT TERM
    restore
    discard
    exit "$status"
}

start() {
    [ "$(id -u)" = "0" ]
    [ -x "$WORKER" ]
    [ -d "$GADGET" ]
    [ ! -e "$LINK" ] && [ ! -L "$LINK" ]
    [ ! -d "$FUNCTION_PATH" ]
    ! mounted
    trap rollback EXIT HUP INT TERM
    mkdir -p "$MOUNT"
    mkdir "$FUNCTION_PATH"
    mount -t functionfs pencast "$MOUNT"
    "$WORKER" --mount "$MOUNT" --fps 60 > /dev/null 2>&1 &
    worker=$!
    echo "$worker" > "$PID"
    ready=0
    attempt=0
    while [ "$attempt" -lt 30 ]; do
        kill -0 "$worker" 2>/dev/null && [ -e "$MOUNT/ep1" ] && [ -e "$MOUNT/ep2" ] && { ready=1; break; }
        sleep 0.1
        attempt=$((attempt + 1))
    done
    [ "$ready" = "1" ]
    old_udc=$(cat "$GADGET/UDC")
    [ -n "$old_udc" ]
    old_product=$(cat "$GADGET/idProduct")
    printf '%s\n' "$old_udc" > "$UDC"
    printf '%s\n' "$old_product" > "$PRODUCT"
    echo "" > "$GADGET/UDC"
    sleep 0.1
    echo "0x0012" > "$GADGET/idProduct"
    (
        cd "$GADGET"
        ln -s "functions/$FUNCTION" "configs/b.1/f3"
    )
    echo "$old_udc" > "$GADGET/UDC"
    trap - EXIT HUP INT TERM
}

stop() {
    [ -d "$GADGET" ]
    if [ -L "$LINK" ] && ! linked; then
        echo "refusing to replace $LINK" >&2
        exit 1
    fi
    if [ -L "$LINK" ] && [ ! -f "$UDC" ]; then
        echo "missing restore state" >&2
        exit 1
    fi
    restore
    discard
}

case "${1:-}" in
    start) start ;;
    stop) stop ;;
    *)
        echo "Usage: $0 {start|stop}" >&2
        exit 2
        ;;
esac
"#;

struct Staging(PathBuf);

impl Staging {
    fn new() -> Result<Self, String> {
        for suffix in 0..64 {
            let path = env::temp_dir().join(format!("pencast-{}-{suffix}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("Creating temporary files failed: {error}")),
            }
        }
        Err(String::from("Creating temporary files failed"))
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) struct TargetSession {
    serial: String,
    active: bool,
}

impl TargetSession {
    pub(crate) fn provision() -> Result<Self, String> {
        let serial = discover_adb_device()?;
        cleanup_stale_session(&serial)?;
        adb_shell(&serial, "mkdir -p /userdata/.pencast\n")?;
        let session = Self {
            serial,
            active: true,
        };
        let staging = Staging::new()?;
        let agent = staging.file("agent.sh");
        let worker = env::current_exe()
            .map_err(|error| error.to_string())?
            .with_file_name("pencast-work");
        if !worker.is_file() {
            return Err(format!("Worker not found: {}", worker.display()));
        }
        fs::write(&agent, AGENT).map_err(|error| format!("Writing agent failed: {error}"))?;
        session.push(&agent, REMOTE_AGENT)?;
        session.push(&worker, "/userdata/.pencast/pencast-work")?;
        adb_shell(
            &session.serial,
            "chmod 700 /userdata/.pencast/agent.sh /userdata/.pencast/pencast-work\n",
        )?;
        match adb_shell_args(
            &session.serial,
            &["/bin/setsid", "/bin/sh", REMOTE_AGENT, "start"],
        ) {
            Ok(_) => Ok(session),
            Err(error) if error.to_ascii_lowercase().contains("closed") => Ok(session),
            Err(error) => Err(error),
        }
    }

    fn push(&self, source: &PathBuf, destination: &str) -> Result<(), String> {
        let output = adb()?
            .args(["-s", self.serial.as_str(), "push"])
            .arg(source)
            .arg(destination)
            .output()
            .map_err(|error| format!("Starting ADB push failed: {error}"))?;
        checked_output("ADB push", output).map(|_| ())
    }

    pub(crate) fn wait_for_usb(&self, running: &AtomicBool) -> Result<Usb, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last_error = String::from("PenCast USB interface not found");
        while Instant::now() < deadline {
            if !running.load(Ordering::Acquire) {
                return Err(String::from("Cancelled"));
            }
            match usb::open() {
                Ok(usb) => return Ok(usb),
                Err(error) => last_error = error,
            }
            thread::sleep(Duration::from_millis(200));
        }
        Err(format!(
            "PenCast USB interface did not appear: {last_error}"
        ))
    }

    pub(crate) fn stop(&mut self) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        adb_shell_args(&self.serial, &["/bin/sh", REMOTE_AGENT, "stop"]).map(|_| ())
    }
}

impl Drop for TargetSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn adb() -> Result<Command, String> {
    Ok(Command::new(
        env::current_exe()
            .map_err(|error| error.to_string())?
            .with_file_name("adb"),
    ))
}

fn output_text(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !text.trim().is_empty() {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    text.trim().to_owned()
}

fn checked_output(operation: &str, output: Output) -> Result<String, String> {
    let text = output_text(&output);
    if output.status.success() {
        Ok(text)
    } else if text.is_empty() {
        Err(format!("{operation} failed: {}", output.status))
    } else {
        Err(format!("{operation} failed: {text}"))
    }
}

fn adb_shell(serial: &str, script: &str) -> Result<String, String> {
    adb_shell_args(serial, &[script])
}

fn adb_shell_args(serial: &str, arguments: &[&str]) -> Result<String, String> {
    let output = adb()?
        .args(["-s", serial, "shell"])
        .args(arguments)
        .output()
        .map_err(|error| format!("Starting ADB shell failed: {error}"))?;
    checked_output("ADB shell", output)
}

fn discover_adb_device() -> Result<String, String> {
    let mut arguments = env::args().skip(1);
    let selected = match (arguments.next(), arguments.next(), arguments.next()) {
        (None, None, None) => None,
        (Some(option), Some(serial), None) if option == "-s" && !serial.is_empty() => Some(serial),
        _ => return Err(String::from("Usage: pencast [-s <serial>]")),
    };
    let output = adb()?
        .arg("devices")
        .output()
        .map_err(|error| format!("Starting ADB failed: {error}"))?;
    let text = checked_output("ADB devices", output)?;
    let mut devices = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(serial) = fields.next() else {
            continue;
        };
        if fields.next() == Some("device") {
            devices.push(serial.to_owned());
        }
    }
    if let Some(serial) = selected {
        if devices.iter().any(|device| device == &serial) {
            return Ok(serial);
        }
        return Err(format!("ADB device is unavailable: {serial}"));
    }
    match devices.as_slice() {
        [] => Err(String::from("No authorized ADB device was found")),
        [serial] => Ok(serial.clone()),
        _ => Err(format!(
            "Multiple ADB devices found:\n{}\nRun: pencast -s <serial>",
            devices.join("\n")
        )),
    }
}

fn cleanup_stale_session(serial: &str) -> Result<(), String> {
    adb_shell(
        serial,
        r#"set -eu
ROOT=/userdata/.pencast
if [ -e "$ROOT" ] || [ -L "$ROOT" ]; then
    if [ -f "$ROOT/agent.sh" ]; then
        /bin/sh "$ROOT/agent.sh" stop
    else
        rm -rf "$ROOT"
    fi
fi
"#,
    )
    .map(|_| ())
}
