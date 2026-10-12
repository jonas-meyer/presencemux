//! The USB gadget of PresenceMux, as a list of configfs operations.

use std::fmt;

/// The configfs name of the UVC function. The cam worker finds the gadget node
/// by the sysfs attribute `function_name`, which shows this name.
const UVC: &str = "uvc.0";
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// The frame interval of the only format, in units of 100 ns.
const FRAME_INTERVAL: u32 = 333_333;
/// `streaming_maxpacket` of the UVC function. It fits the default dwc2 TX
/// FIFOs of the Raspberry Pi 5.
const MAX_PACKET: u32 = 2048;

/// One configfs operation, with paths relative to the gadget directory.
#[derive(Debug)]
pub(crate) enum Op {
    Dir(String),
    Write {
        path: String,
        value: String,
    },
    /// A symlink at `link` to `target`.
    Link {
        target: String,
        link: String,
    },
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dir(path) if path.is_empty() => write!(f, "mkdir ."),
            Self::Dir(path) => write!(f, "mkdir {path}"),
            Self::Write { path, value } => write!(f, "write {path} = {value}"),
            Self::Link { target, link } => write!(f, "link {link} -> {target}"),
        }
    }
}

/// Returns the operations that make the gadget, in order. The UDC binding is
/// not part of them.
pub(crate) fn plan(serial: &str) -> Vec<Op> {
    let mut plan = Plan::default();
    plan.dir("");
    plan.write("idVendor", "0x1d6b");
    plan.write("idProduct", "0x0104");
    plan.write("bcdDevice", "0x0001");
    plan.write("bDeviceClass", "0xEF");
    plan.write("bDeviceSubClass", "0x02");
    plan.write("bDeviceProtocol", "0x01");
    plan.write("max_speed", "high-speed");
    plan.dir("strings/0x409");
    plan.write("strings/0x409/manufacturer", "PresenceMux");
    plan.write("strings/0x409/product", "PresenceMux Studio");
    plan.write("strings/0x409/serialnumber", serial);

    plan.dir("configs/c.1");
    plan.dir("configs/c.1/strings/0x409");
    plan.write("configs/c.1/strings/0x409/configuration", "PresenceMux");
    plan.write("configs/c.1/bmAttributes", "0xC0");
    plan.write("configs/c.1/MaxPower", "100");

    uvc(&mut plan);
    microphone(&mut plan);
    speakers(&mut plan);
    plan.ops
}

/// Adds the UVC function with its only format.
fn uvc(plan: &mut Plan) {
    let f = format!("functions/{UVC}");
    plan.dir(&f);
    plan.write(&format!("{f}/function_name"), "Studio Camera");
    plan.write(&format!("{f}/streaming_maxpacket"), &MAX_PACKET.to_string());
    plan.write(&format!("{f}/streaming_interval"), "1");

    let format = format!("{f}/streaming/mjpeg/m");
    let frame = format!("{format}/1080p");
    plan.dir(&format);
    plan.dir(&frame);
    plan.write(&format!("{frame}/wWidth"), &WIDTH.to_string());
    plan.write(&format!("{frame}/wHeight"), &HEIGHT.to_string());
    plan.write(
        &format!("{frame}/dwFrameInterval"),
        &FRAME_INTERVAL.to_string(),
    );
    plan.write(
        &format!("{frame}/dwDefaultFrameInterval"),
        &FRAME_INTERVAL.to_string(),
    );
    plan.write(
        &format!("{frame}/dwMinBitRate"),
        &(WIDTH * HEIGHT * 80).to_string(),
    );
    plan.write(
        &format!("{frame}/dwMaxBitRate"),
        &(WIDTH * HEIGHT * 160).to_string(),
    );
    plan.write(
        &format!("{frame}/dwMaxVideoFrameBufferSize"),
        &(WIDTH * HEIGHT * 2).to_string(),
    );

    let header = format!("{f}/streaming/header/h");
    plan.dir(&header);
    plan.link(&format, &format!("{header}/m"));
    for speed in ["fs", "hs", "ss"] {
        plan.link(&header, &format!("{f}/streaming/class/{speed}/h"));
    }
    let control = format!("{f}/control/header/h");
    plan.dir(&control);
    // The control class has no high-speed group. High speed uses fs.
    for speed in ["fs", "ss"] {
        plan.link(&control, &format!("{f}/control/class/{speed}/h"));
    }
    plan.link(&f, &format!("configs/c.1/{UVC}"));
}

/// Adds the UAC2 function of the microphone. In `f_uac2`, `p_` is the stream
/// from the gadget to the host.
fn microphone(plan: &mut Plan) {
    let f = "functions/uac2.mic";
    plan.dir(f);
    audio_common(plan, f);
    plan.write(&format!("{f}/p_chmask"), "1");
    plan.write(&format!("{f}/c_chmask"), "0");
    plan.write(&format!("{f}/function_name"), "Studio Microphone");
    plan.write(&format!("{f}/if_ctrl_name"), "Studio Microphone");
    plan.write(&format!("{f}/p_it_name"), "Studio Microphone");
    plan.write(&format!("{f}/clksrc_in_name"), "Studio Microphone");
    plan.write(&format!("{f}/clksrc_out_name"), "Studio Microphone");
    plan.link(f, "configs/c.1/uac2.mic");
}

/// Adds the UAC2 function of the speakers. In `f_uac2`, `c_` is the stream from
/// the host to the gadget.
fn speakers(plan: &mut Plan) {
    let f = "functions/uac2.spk";
    plan.dir(f);
    audio_common(plan, f);
    plan.write(&format!("{f}/p_chmask"), "0");
    plan.write(&format!("{f}/c_chmask"), "3");
    plan.write(&format!("{f}/function_name"), "Studio Audio");
    plan.write(&format!("{f}/if_ctrl_name"), "Studio Audio");
    plan.write(&format!("{f}/c_ot_name"), "Studio Audio");
    plan.write(&format!("{f}/clksrc_in_name"), "Studio Audio");
    plan.write(&format!("{f}/clksrc_out_name"), "Studio Audio");
    plan.link(f, "configs/c.1/uac2.spk");
}

/// Adds the settings that both UAC2 functions share.
fn audio_common(plan: &mut Plan, f: &str) {
    // Both functions need the same single rate and no mute or volume control.
    for (attribute, value) in [
        ("p_srate", "48000"),
        ("c_srate", "48000"),
        ("p_ssize", "2"),
        ("c_ssize", "2"),
        ("c_sync", "async"),
        ("p_mute_present", "0"),
        ("p_volume_present", "0"),
        ("c_mute_present", "0"),
        ("c_volume_present", "0"),
        ("p_hs_bint", "4"),
        ("c_hs_bint", "4"),
        ("fb_max", "5"),
        ("req_number", "4"),
    ] {
        plan.write(&format!("{f}/{attribute}"), value);
    }
}

#[derive(Default)]
struct Plan {
    ops: Vec<Op>,
}

impl Plan {
    fn dir(&mut self, path: &str) {
        self.ops.push(Op::Dir(path.to_owned()));
    }

    fn write(&mut self, path: &str, value: &str) {
        self.ops.push(Op::Write {
            path: path.to_owned(),
            value: value.to_owned(),
        });
    }

    fn link(&mut self, target: &str, link: &str) {
        self.ops.push(Op::Link {
            target: target.to_owned(),
            link: link.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use core::fmt::Write;

    use super::*;

    #[test]
    fn plan_is_the_reviewed_gadget() {
        let mut table = String::new();
        for op in plan("0123456789abcdef") {
            writeln!(table, "{op}").unwrap();
        }

        insta::assert_snapshot!(table);
    }
}
