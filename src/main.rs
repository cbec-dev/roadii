use anyhow::{anyhow, bail};
use anyhow::{Context, Result};
use clap::Parser;
use std::ffi::OsString;
use std::path::PathBuf;
use udev::{Device, Enumerator, Udev};

/// Wii Guitar mapping utility
#[derive(Parser, Debug)]
struct Args {
    /// The kernel name of the device to match, for example `input19`.
    /// If it is a Wiimote with a guitar attached it will be remapped.
    ///
    /// If not supplied, the stable udev symlinks (/dev/input/wiitar-wiimote,
    /// -guitar and -accel, created by the shipped udev rules) are used
    /// instead, and evsieve is run with persist=reopen so the virtual device
    /// survives the guitar disconnecting and reconnecting.
    #[arg(short, long)]
    kernel_name: Option<OsString>,

    /// The path to the `evsieve` binary, useful if it isn't
    /// available in the `PATH` environment variable.
    ///
    /// If not supplied, `evsieve` will be run from the PATH.
    #[arg(short, long)]
    evsieve_path: Option<PathBuf>,

    /// The name to give the virtual output device.
    ///
    /// If not supplied, a friendly name is looked up from the controller's
    /// Bluetooth MAC in a names file (see `lookup_name`), otherwise it
    /// defaults to `Wiitar <MAC>` so that multiple guitars connected at once
    /// each produce a distinct, stable device.
    #[arg(short, long)]
    output_name: Option<String>,

    /// How long, in seconds, the virtual device outlives a guitar disconnect
    /// in symlink mode (no --kernel-name). Within this window a reconnecting
    /// guitar resumes as the same device, so games that don't hot-plug keep
    /// working; past it, evsieve is stopped and the device removed (udev
    /// starts the service again on the next connect).
    #[arg(short, long, default_value_t = 30)]
    grace_period: u64,
}

/// A recorded evsieve command line, so it can either be exec'd (one-shot
/// kernel-name mode) or spawned and supervised (symlink mode).
struct Cmd {
    program: OsString,
    args: Vec<OsString>,
}

impl Cmd {
    fn new(program: impl Into<OsString>) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
        }
    }

    fn arg(&mut self, arg: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    fn args<S: AsRef<std::ffi::OsStr>>(&mut self, args: &[S]) -> &mut Self {
        for arg in args {
            self.arg(arg);
        }
        self
    }
}

#[derive(Debug, Default)]
struct Wiitar {
    /// A stable identifier for the physical controller, taken from the
    /// Bluetooth address (`uniq`) of the guitar input device. Used to give
    /// the virtual output device a deterministic name.
    id: Option<String>,
    wiimote: Option<Device>,
    guitar: Option<Device>,
    accel: Option<Device>,
}

impl Wiitar {
    fn from_kernel_name(kernel_name: OsString) -> Result<Self> {
        let udev = Udev::new().context("couldn't get access to Udev")?;

        Self::from_kernel_name_with_udev(kernel_name, udev)
    }

    fn from_kernel_name_with_udev(kernel_name: OsString, udev: Udev) -> Result<Self> {
        let guitar = {
            let mut kernel_name_enumerator = Enumerator::with_udev(udev.clone())
                .context("couldn't start a device enumerator")?;
            kernel_name_enumerator
                .match_sysname(&kernel_name)
                .unwrap_or_else(|_| {
                    panic!("couldn't set {:?} as parent device matcher", kernel_name)
                });

            let matching_devices: Vec<Device> = kernel_name_enumerator
                .scan_devices()
                .context("couldn't scan devices")?
                .collect();

            if matching_devices.len() != 1 {
                bail!(
                    "couldn't find a single matching device for {:?}",
                    kernel_name
                );
            }

            matching_devices[0].clone()
        };

        {
            // First up, we want to bail if this device doesn't pass our basic
            // sniff test. Theoretically the udev rule should guard against
            // this too but better to make sure than not!
            let name = guitar
                .attribute_value("name")
                .context("This device has no name? That's very strange.")?
                .to_string_lossy();

            // Unfortunately, despite an `extension` attribute on the hid-wiimote
            // driver, it isn't accessible after mount, so we may need to rely on
            // the display name, which is kind of strange, but if it works?
            if !name.contains("Wii") || !name.ends_with("Guitar") {
                bail!("That's a weird looking Wii Guitar (are the udev rules set right?)");
            }
        }

        // Next, we need to look at the parent device. Ultimately we want to
        // operate on the guitar device's siblings, but to get those we first
        // need to look at the parent, so, here we go...
        let wiimote = guitar
            .parent()
            .context("guitar didn't have a parent device")?;

        {
            // Sanity checks; the parent should be a hid-wiimote device
            if wiimote
                .subsystem()
                .context("The parent of the wiitar didn't have a subsystem")?
                != "hid"
            {
                bail!("The parent of the Wiitar is not a HID device?");
            }

            if wiimote
                .driver()
                .context("The parent of the wiitar didn't have a driver")?
                != "wiimote"
            {
                bail!("The parent of the Wiitar is an HID device but not a Wiimote?");
            }
        }

        println!(
            "Looks like {} is a Wiimote, with a guitar attached at {}!",
            wiimote.sysname().to_string_lossy(),
            guitar.sysname().to_string_lossy()
        );

        // Cool, let's get the party started, now we initialise our struct
        let mut inputs: Self = Default::default();

        // Grab a stable identifier for this physical controller: the Wiimote's
        // Bluetooth MAC, which is constant across reconnects unlike the kernel
        // input index. On BlueZ/uhid the input devices carry an empty `uniq`,
        // but the parent HID device exposes the MAC as the `HID_UNIQ` udev
        // property, so we prefer that and fall back to the `uniq` attributes.
        inputs.id = wiimote
            .property_value("HID_UNIQ")
            .or_else(|| guitar.attribute_value("uniq"))
            .or_else(|| wiimote.attribute_value("uniq"))
            .map(|value| value.to_string_lossy().into_owned())
            .filter(|value| !value.is_empty());

        {
            // Now we want to query siblings of the guitar
            let mut sibling_enumerator = Enumerator::with_udev(udev.clone())
                .context("couldn't start a device enumerator")?;
            sibling_enumerator
                .match_parent(&wiimote)
                .context("couldn't set wiimote as parent device matcher")?;
            sibling_enumerator
                .match_subsystem("input")
                .context("couldn't set input as device subsystem matcher")?;

            for device in sibling_enumerator
                .scan_devices()
                .context("couldn't scan sibling devices")?
                .filter(|device| {
                    device.syspath() != wiimote.syspath()
                        && device.parent().expect("device had no parent").syspath()
                            == wiimote.syspath()
                })
            {
                // Like mentioned above, the name is the best we can match
                // these on, thankfully these strings are constants in the
                // Linux kernel, and unlikely to change much, if at all.
                match device.attribute_value("name") {
                    Some(os_name) => match os_name.to_string_lossy().into_owned().as_str() {
                        "Nintendo Wii Remote" => {
                            if inputs.wiimote.is_none() {
                                let wiimote = Self::get_event_device_from_input_device_with_udev(
                                    &device,
                                    udev.clone(),
                                )?;
                                inputs.wiimote = Some(wiimote);
                            }
                        }
                        "Nintendo Wii Remote Guitar" => {
                            if inputs.guitar.is_none() {
                                let guitar = Self::get_event_device_from_input_device_with_udev(
                                    &device,
                                    udev.clone(),
                                )?;
                                inputs.guitar = Some(guitar);
                            }
                        }
                        "Nintendo Wii Remote Accelerometer" => {
                            if inputs.accel.is_none() {
                                let accel = Self::get_event_device_from_input_device_with_udev(
                                    &device,
                                    udev.clone(),
                                )?;
                                inputs.accel = Some(accel);
                            }
                        }
                        &_ => continue,
                    },
                    None => continue,
                };

                if inputs.is_complete() {
                    break;
                }
            }
        }

        if !inputs.is_complete() {
            bail!("Failed to find wiimote, guitar and accelerometer input devices");
        }

        Ok(inputs)
    }

    fn get_event_device_from_input_device_with_udev(device: &Device, udev: Udev) -> Result<Device> {
        let mut enumerator =
            Enumerator::with_udev(udev).context("couldn't start a device enumerator")?;
        enumerator
            .match_parent(device)
            .context("couldn't set device as parent device matcher")?;
        enumerator
            .match_subsystem("input")
            .context("couldn't set event as device subsystem matcher")?;

        for child in enumerator
            .scan_devices()
            .context("couldn't scan sibling devices")?
        {
            if child.syspath() == device.syspath() {
                continue;
            }

            if child.sysname().to_string_lossy().starts_with("event") {
                return Ok(child);
            }
        }

        bail!("didn't find a child event device")
    }

    fn is_complete(&self) -> bool {
        self.wiimote.is_some() && self.guitar.is_some() && self.accel.is_some()
    }
}

/// Path to an optional file mapping controller Bluetooth MACs to friendly
/// device names, one `MAC = Name` pair per line. Lines starting with `#` and
/// blank lines are ignored. Example:
///
/// ```text
/// # ~/.config/roadii/names.conf  (or /etc/roadii/names.conf)
/// 00:1F:32:12:34:56 = Player 1
/// 00:1F:32:AB:CD:EF = Player 2
/// ```
fn names_files() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(dir).join("roadii/names.conf"));
    } else if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".config/roadii/names.conf"));
    }
    paths.push(PathBuf::from("/etc/roadii/names.conf"));
    paths
}

/// Look up a friendly name for the given controller id (MAC) in the first
/// names file that both exists and contains a matching entry. MAC comparison
/// is case-insensitive.
fn lookup_name(id: &str) -> Option<String> {
    let id = id.trim();
    for path in names_files() {
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(_) => continue,
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((mac, name)) = line.split_once('=') else {
                continue;
            };
            if mac.trim().eq_ignore_ascii_case(id) {
                let name = name.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// Stable device symlinks created by the shipped udev rules. They always
/// point at the Wiimote's current event nodes, so evsieve can reopen them
/// after a reconnect even if the kernel hands out different event numbers.
const LINK_WIIMOTE: &str = "/dev/input/wiitar-wiimote";
const LINK_GUITAR: &str = "/dev/input/wiitar-guitar";
const LINK_ACCEL: &str = "/dev/input/wiitar-accel";

/// Find the controller's Bluetooth MAC starting from the guitar symlink:
/// resolve it to the event node, then walk up the udev parents to the HID
/// device carrying the `HID_UNIQ` property.
fn id_from_guitar_link() -> Option<String> {
    let target = std::fs::canonicalize(LINK_GUITAR).ok()?;
    let sysname = target.file_name()?.to_str()?.to_owned();
    let syspath = PathBuf::from(format!("/sys/class/input/{}", sysname));
    let mut device = Device::from_syspath(&syspath).ok()?;
    loop {
        if let Some(id) = device
            .property_value("HID_UNIQ")
            .map(|value| value.to_string_lossy().into_owned())
            .filter(|value| !value.is_empty())
        {
            return Some(id);
        }
        device = device.parent()?;
    }
}

fn main() -> Result<()> {
    // We put this in a block so the main function can drop
    // everything else afterwards in preparation for exec'ing
    let evsieve = {
        let args = Args::parse();

        // Resolve the three input devices. With an explicit kernel name we
        // look the devices up through udev and exit when the guitar goes
        // away (the udev rule starts a fresh instance per connection). With
        // no kernel name we rely on the stable udev symlinks and stay alive
        // across reconnects, so games keep seeing one uninterrupted
        // controller instead of a new one per reconnect.
        let (wiimote_path, guitar_path, accel_path, id, persist, fallback_name);
        let grace: Option<std::time::Duration>;
        match args.kernel_name {
            Some(kernel_name) => {
                grace = None;
                fallback_name = format!("Wiitar {}", kernel_name.to_string_lossy());
                let parts = Wiitar::from_kernel_name(kernel_name)?;
                let devnode = |device: Option<Device>, what: &str| -> Result<PathBuf> {
                    Ok(device
                        .ok_or_else(|| anyhow!("missing {}", what))?
                        .devnode()
                        .ok_or_else(|| anyhow!("failed to retrieve {} devnode", what))?
                        .to_owned())
                };
                wiimote_path = devnode(parts.wiimote, "wiimote")?;
                guitar_path = devnode(parts.guitar, "wiimote guitar")?;
                accel_path = devnode(parts.accel, "wiimote accelerometer")?;
                id = parts.id;
                persist = "persist=exit";
            }
            None => {
                for link in [LINK_WIIMOTE, LINK_GUITAR, LINK_ACCEL] {
                    if !PathBuf::from(link).exists() {
                        bail!(
                            "{} not found; is the guitar connected and are the udev rules installed?",
                            link
                        );
                    }
                }
                grace = Some(std::time::Duration::from_secs(args.grace_period));
                fallback_name = "Wiitar".to_string();
                wiimote_path = PathBuf::from(LINK_WIIMOTE);
                guitar_path = PathBuf::from(LINK_GUITAR);
                accel_path = PathBuf::from(LINK_ACCEL);
                id = id_from_guitar_link();
                persist = "persist=reopen";
            }
        }

        // Work out the output device name. Precedence:
        //   1. An explicit --output-name from the caller.
        //   2. A friendly label mapped from this controller's Bluetooth MAC
        //      in the names file (e.g. so a given guitar is always "Player 1").
        //   3. "Wiitar <MAC>", which is stable across reconnects.
        //   4. "Wiitar <kernel-name>" (or plain "Wiitar" in symlink mode) if
        //      we couldn't read a MAC at all.
        let output_name = args
            .output_name
            .or_else(|| id.as_deref().and_then(lookup_name))
            .or_else(|| id.as_ref().map(|id| format!("Wiitar {}", id)))
            .unwrap_or(fallback_name);

        println!("Creating output device named {:?}", output_name);

        let mut evsieve = Cmd::new(args.evsieve_path.unwrap_or("evsieve".into()));

        evsieve
            .arg("--input")
            .arg(&wiimote_path)
            .args(&["domain=wiimote", "grab", persist]);

        evsieve.args(&["--map", "btn:south@wiimote", "btn:mode@wiitar"]);
        // BTN_C rather than BTN_THUMBL: every real Xbox 360 button is spoken
        // for, and Wine hides buttons a real 360 pad doesn't have, so the
        // visible THUMBL goes to strum down (which GHWT:DE must bind) and the
        // rarely-used Wiimote "1" gets the invisible-under-Wine BTN_C.
        evsieve.args(&["--map", "btn:1@wiimote", "btn:c@wiitar"]);
        evsieve.args(&["--map", "btn:2@wiimote", "btn:thumbr@wiitar"]);
        evsieve.args(&["--map", "btn:mode@wiimote", "btn:z@wiitar"]);
        evsieve.args(&["--map", "key:next@wiimote", "btn:start@wiitar"]);
        evsieve.args(&["--map", "key:previous@wiimote", "btn:select@wiitar"]);
        evsieve.args(&["--map", "key:left@wiimote", "btn:dpad_up@wiitar"]);
        evsieve.args(&["--map", "key:right@wiimote", "btn:dpad_down@wiitar"]);
        evsieve.args(&["--map", "key:up@wiimote", "btn:dpad_left@wiitar"]);
        evsieve.args(&["--map", "key:down@wiimote", "btn:dpad_right@wiitar"]);

        evsieve
            .arg("--input")
            .arg(&guitar_path)
            .args(&["domain=guitar", "grab", persist]);

        evsieve.args(&["--map", "btn:south@wiimote", "btn:mode@wiitar"]);
        evsieve.args(&["--map", "btn:1@guitar", "btn:south@wiitar"]);
        evsieve.args(&["--map", "btn:2@guitar", "btn:east@wiitar"]);
        evsieve.args(&["--map", "btn:3@guitar", "btn:west@wiitar"]);
        evsieve.args(&["--map", "btn:4@guitar", "btn:north@wiitar"]);
        evsieve.args(&["--map", "btn:5@guitar", "btn:tl@wiitar"]);
        evsieve.args(&["--map", "btn:start@guitar", "btn:start@wiitar"]);
        evsieve.args(&["--map", "btn:select@guitar", "btn:select@wiitar"]);
        // The strum bar reports as BTN_DPAD_UP / BTN_DPAD_DOWN key events,
        // mapped straight through to match a CRKD guitar (which reports
        // strum as an actual d-pad axis natively). Note this may not work in
        // GHWT:DE via Wine, which hides d-pad keys and any button a real
        // Xbox 360 pad doesn't have.
        evsieve.args(&["--map", "btn:dpad_up@guitar", "btn:dpad_up@wiitar"]);
        evsieve.args(&["--map", "btn:dpad_down@guitar", "btn:dpad_down@wiitar"]);
        // The whammy drives the left stick's Y axis, on top of the neck
        // pot's own X/Y mapping below (both write abs:y@wiitar; whichever
        // moved more recently wins). Source ABS_HAT1X declares 0..15 but,
        // like the neck pot below, physically only reaches ~12, so rescale
        // from the measured reach and clamp rather than the declared max.
        // Measured on hardware: idle sits at 3 (jittering 1..3 at rest, not
        // 0), and full whammy reaches 12. Rebase around that idle point so
        // rest maps to dead center, and snap anything at or below idle to 0
        // so pot jitter doesn't nudge the stick off center.
        const IDLE: f64 = 3.0;
        const REACH: f64 = 12.0 - IDLE;
        for v in 0i32..=15 {
            let target = if f64::from(v) <= IDLE {
                0
            } else {
                ((f64::from(v) - IDLE) * -32.0 / REACH).round().clamp(-32.0, 0.0) as i32
            };
            evsieve.args(&[
                "--map".to_string(),
                format!("abs:hat1x:{}@guitar", v),
                format!("abs:y:{}@wiitar", target),
            ]);
        }
        // The stick's pot occasionally emits a single spurious full-deflection
        // reading of the opposite sign mid-hold. Real motion moves at most ~4
        // raw units per event while a glitch jumps ~15, so drop any event
        // that leaps across the center by more than 10 units in one step.
        for axis in ["x", "y"] {
            evsieve.args(&["--block".to_string(), format!("abs:{}:5~..~-5@guitar", axis)]);
            evsieve.args(&["--block".to_string(), format!("abs:{}:~-5..5~@guitar", axis)]);
        }
        // The stick only physically reaches a fraction of its declared ±32
        // range, and asymmetrically at that (measured: -6..+10 raw on X,
        // -9..+7 on Y), so games see a stick that barely leaves the deadzone.
        // A scale factor doesn't help: evsieve scales the declared range
        // together with the values, keeping the relative reach the same.
        // Instead, emit one map per raw value, rescaled per direction so the
        // measured physical extremes hit the range bounds. Values ±1 around
        // center snap to 0 so the rest position is centered.
        for (axis, neg_reach, pos_reach) in [("x", 5.0_f64, 10.0_f64), ("y", 9.0, 7.0)] {
            for v in -32i32..=31 {
                let target = if v.abs() <= 1 {
                    0
                } else {
                    let reach = if v < 0 { neg_reach } else { pos_reach };
                    (f64::from(v) * 32.0 / reach).round().clamp(-32.0, 31.0) as i32
                };
                evsieve.args(&[
                    "--map".to_string(),
                    format!("abs:{}:{}@guitar", axis, v),
                    format!("abs:{}:{}@wiitar", axis, target),
                ]);
            }
        }

        evsieve
            .arg("--input")
            .arg(&accel_path)
            .args(&["domain=accel", "grab", persist]);

        evsieve.args(&["--block", "abs:rz@accel", "abs:rx@accel"]);
        // Tilt (star power) drives the right-trigger axis (ABS_RZ), snapping
        // 0 -> 255. A CRKD guitar reports tilt as ABS_HAT0X instead, but
        // evsieve can't create that as a synthetic output axis here: unlike
        // ABS_RZ (which the accel device already has natively, just blocked
        // above), there's no source device with an ABS_HAT0X capability for
        // evsieve to infer a valid range from, so uinput device creation
        // fails outright. ABS_RZ is a strict axis, so it can't collide with
        // any button binding. Press at -90 but only release back at -78: the
        // deadband keeps a slight wobble around the threshold from
        // toggling star power repeatedly.
        // Also fires dpad_left, matching how a CRKD guitar reports tilt (as a
        // d-pad direction rather than a trigger axis), for games that bind
        // to that instead. Shares btn:dpad_left@wiitar with the Wiimote's
        // own d-pad above; whichever fires more recently wins. Both
        // destinations must be listed on the same --map: separate --map
        // calls on the same source only let the first one claim the event.
        evsieve.args(&[
            "--map",
            "abs:ry:-89~..~-90@accel",
            "abs:rz:255@wiitar",
            "btn:dpad_left:1@wiitar",
        ]);
        evsieve.args(&[
            "--map",
            "abs:ry:~-78..-77~@accel",
            "abs:rz:0@wiitar",
            "btn:dpad_left:0@wiitar",
        ]);

        // Masquerade as a wired Xbox 360 controller (vendor 045e, product
        // 028e, USB bus). SDL — which YARG and other SDL_GameController-based
        // games use — only keeps a joystick around if it can match its
        // vendor:product GUID to a controller mapping. Without a device-id the
        // device shows up as 0000:0000, fails to map, and SDL immediately drops
        // it (Clone Hero reads raw joystick input, so it works there anyway).
        // SDL ships a built-in mapping for the 360 pad, and our button/axis
        // codes already match the xpad layout it expects.
        evsieve
            .arg("--output")
            .arg(format!("name={}", output_name))
            .args(&["device-id=045e:028e", "bus=3", "version=110"])
            .arg("@wiitar");

        (evsieve, grace)
    };

    let (evsieve, grace) = evsieve;
    match grace {
        // Symlink mode: supervise evsieve so the virtual device outlives
        // brief disconnects but is torn down after the grace period.
        Some(grace) => supervise(evsieve, grace),
        // Kernel-name mode: hand the process over to evsieve entirely.
        None => {
            let mut command = exec::Command::new(&evsieve.program);
            command.args(&evsieve.args);
            Err(command.exec().into())
        }
    }
}

/// True if the process holds an open file descriptor to `target` (a resolved
/// device node), judging by /proc/<pid>/fd.
fn process_has_open(pid: u32, target: &std::path::Path) -> bool {
    let fd_dir = PathBuf::from(format!("/proc/{}/fd", pid));
    let Ok(entries) = std::fs::read_dir(fd_dir) else {
        return false;
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .any(|link| link == target)
}

/// True if evsieve (pid) holds all three input devices the symlinks
/// currently point at. evsieve 1.4.0's persist=reopen gives up on a device
/// permanently if its single reopen attempt fails (e.g. the node is briefly
/// grabbed by another program, or hid-wiimote is still initialising when the
/// symlink appears), so after a reconnect an input — in practice the
/// accelerometer, whose node appears instantly and races other openers — can
/// silently stay dead while the rest keep working.
fn all_inputs_attached(pid: u32) -> bool {
    [LINK_WIIMOTE, LINK_GUITAR, LINK_ACCEL].iter().all(|link| {
        match std::fs::canonicalize(link) {
            Ok(node) => process_has_open(pid, &node),
            Err(_) => false,
        }
    })
}

/// Run evsieve as a child process and watch the guitar symlink, which udev
/// removes on disconnect and recreates on reconnect. While the guitar is
/// away, evsieve (persist=reopen) keeps the virtual device alive, so a quick
/// reconnect resumes seamlessly and running games keep their controller. If
/// the guitar stays away past the grace period, stop evsieve and exit; udev
/// starts the service again on the next connect.
///
/// After a reconnect, verify evsieve actually reattached all three inputs;
/// if one lost the reopen race (see all_inputs_attached), restart evsieve so
/// tilt and friends come back, at the cost of recreating the virtual device.
fn supervise(evsieve: Cmd, grace: std::time::Duration) -> Result<()> {
    use std::time::Instant;

    let spawn = || {
        std::process::Command::new(&evsieve.program)
            .args(&evsieve.args)
            .spawn()
            .context("failed to spawn evsieve")
    };
    let mut child = spawn()?;

    // How long evsieve gets to reopen everything after a reconnect before we
    // conclude an input was dropped and restart it.
    const REATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let mut gone_since: Option<Instant> = None;
    let mut reattach_deadline: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait().context("failed to poll evsieve")? {
            bail!("evsieve exited unexpectedly: {}", status);
        }

        // Path::exists follows symlinks, so a dangling link counts as gone.
        if PathBuf::from(LINK_GUITAR).exists() {
            if gone_since.take().is_some() {
                println!("Guitar reconnected within the grace period, carrying on");
                reattach_deadline = Some(Instant::now() + REATTACH_TIMEOUT);
            }

            if let Some(deadline) = reattach_deadline {
                if all_inputs_attached(child.id()) {
                    reattach_deadline = None;
                } else if Instant::now() > deadline
                    && [LINK_WIIMOTE, LINK_GUITAR, LINK_ACCEL]
                        .iter()
                        .all(|link| PathBuf::from(link).exists())
                {
                    println!(
                        "evsieve didn't reattach all inputs after the reconnect, restarting it"
                    );
                    child.kill().context("failed to stop evsieve")?;
                    child.wait().context("failed to reap evsieve")?;
                    child = spawn()?;
                    reattach_deadline = None;
                }
            }
        } else {
            let since = *gone_since.get_or_insert_with(|| {
                println!(
                    "Guitar disconnected, keeping the virtual device for {}s",
                    grace.as_secs()
                );
                Instant::now()
            });
            if since.elapsed() > grace {
                println!("Guitar didn't come back, shutting down");
                child.kill().context("failed to stop evsieve")?;
                child.wait().context("failed to reap evsieve")?;
                return Ok(());
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
