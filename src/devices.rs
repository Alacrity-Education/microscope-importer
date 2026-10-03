//! SD card discovery through `lsblk`, which sees cards whether or not they
//! are mounted.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// One filesystem on a removable device - normally the single partition of
/// an SD card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// Stable identity across polls: device node plus filesystem UUID.
    pub key: String,
    /// /dev/sdc1
    pub dev: String,
    /// sdc1
    pub name: String,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub fstype: String,
    pub size: u64,
    pub mountpoint: Option<PathBuf>,
    /// Reader / card model as reported by the kernel.
    pub model: String,
}

impl Device {
    /// Folder name for this card's originals: the filesystem UUID if it has
    /// one, which survives re-insertion and changes only on reformatting.
    pub fn card_id(&self) -> String {
        let raw = self
            .uuid
            .clone()
            .or_else(|| self.label.clone())
            .unwrap_or_else(|| self.name.clone());
        raw.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
}

const SYSTEM_MOUNTS: &[&str] = &[
    "/", "/boot", "/efi", "/home", "/usr", "/var", "/srv", "[SWAP]",
];
const NOT_FILESYSTEMS: &[&str] = &["swap", "crypto_LUKS", "LVM2_member", "linux_raid_member"];

fn flag(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_u64() == Some(1),
        Value::String(s) => s == "1" || s == "true",
        _ => false,
    }
}

fn number(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn mountpoints(node: &Value) -> Vec<String> {
    let mut out: Vec<String> = node["mountpoints"]
        .as_array()
        .map(|a| a.iter().filter_map(text).collect())
        .unwrap_or_default();
    if let Some(m) = text(&node["mountpoint"]) {
        out.push(m);
    }
    out
}

fn walk<'a>(node: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(node);
    if let Some(children) = node["children"].as_array() {
        for c in children {
            walk(c, out);
        }
    }
}

fn is_system_mount(m: &str) -> bool {
    SYSTEM_MOUNTS
        .iter()
        .any(|s| m == *s || (*s != "/" && m.starts_with(&format!("{s}/"))))
}

/// Parse `lsblk -J -b -o ...` output into the removable filesystems on it.
pub fn parse(json: &str, include_loop: bool) -> Result<Vec<Device>> {
    let root: Value = serde_json::from_str(json).context("unexpected lsblk output")?;
    let Some(disks) = root["blockdevices"].as_array() else {
        bail!("unexpected lsblk output: no blockdevices");
    };
    let mut devices = Vec::new();
    for disk in disks {
        let name = text(&disk["name"]).unwrap_or_default();
        let kind = text(&disk["type"]).unwrap_or_default();
        let tran = text(&disk["tran"]).unwrap_or_default();
        let removable = flag(&disk["rm"])
            || flag(&disk["hotplug"])
            || matches!(tran.as_str(), "usb" | "mmc")
            || name.starts_with("mmcblk");
        let wanted = match kind.as_str() {
            "disk" => removable,
            "loop" => include_loop,
            _ => false,
        };
        if !wanted {
            continue;
        }
        let mut nodes = Vec::new();
        walk(disk, &mut nodes);
        // Never offer a disk that carries the running system.
        if nodes
            .iter()
            .flat_map(|n| mountpoints(n))
            .any(|m| is_system_mount(&m))
        {
            continue;
        }
        let model = [text(&disk["vendor"]), text(&disk["model"])]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        for node in nodes {
            let Some(fstype) = text(&node["fstype"]) else {
                continue;
            };
            if NOT_FILESYSTEMS.contains(&fstype.as_str()) {
                continue;
            }
            let Some(dev) = text(&node["path"]) else {
                continue;
            };
            let uuid = text(&node["uuid"]);
            devices.push(Device {
                key: format!("{dev}|{}", uuid.clone().unwrap_or_default()),
                name: text(&node["name"]).unwrap_or_else(|| dev.clone()),
                dev,
                label: text(&node["label"]),
                uuid,
                fstype,
                size: number(&node["size"]),
                mountpoint: mountpoints(node).into_iter().next().map(PathBuf::from),
                model: model.clone(),
            });
        }
    }
    Ok(devices)
}

const COLUMNS: &str =
    "NAME,PATH,TYPE,SIZE,RM,HOTPLUG,TRAN,FSTYPE,LABEL,UUID,MOUNTPOINTS,MODEL,VENDOR";

fn lsblk(extra: &[&str]) -> Result<String> {
    let out = Command::new("lsblk")
        .args(["-J", "-b", "-o", COLUMNS])
        .args(extra)
        .output()
        .context("cannot run lsblk")?;
    if !out.status.success() {
        bail!(
            "lsblk failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Loop devices are offered only when MICROSCOPE_IMPORTER_LOOP=1, so a card
/// image (`udisksctl loop-setup -f card.img`) can stand in for a real card.
pub fn include_loop() -> bool {
    std::env::var("MICROSCOPE_IMPORTER_LOOP").is_ok_and(|v| v == "1")
}

/// All removable filesystems currently attached, mounted or not.
pub fn list() -> Result<Vec<Device>> {
    parse(&lsblk(&[])?, include_loop())
}

/// Current mount point of one device node.
pub fn mountpoint_of(dev: &str) -> Option<PathBuf> {
    let root: Value = serde_json::from_str(&lsblk(&[dev]).ok()?).ok()?;
    let mut nodes = Vec::new();
    for d in root["blockdevices"].as_array()? {
        walk(d, &mut nodes);
    }
    nodes
        .into_iter()
        .find(|n| text(&n["path"]).as_deref() == Some(dev))
        .and_then(|n| mountpoints(n).into_iter().next())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"blockdevices":[
      {"name":"sdb","path":"/dev/sdb","type":"disk","size":120034123776,"rm":false,"hotplug":false,"tran":"sata","fstype":null,"label":null,"uuid":null,"mountpoints":[null],"model":"Patriot","vendor":"ATA",
       "children":[{"name":"sdb1","path":"/dev/sdb1","type":"part","size":1127219200,"rm":false,"hotplug":false,"tran":null,"fstype":"vfat","label":null,"uuid":"A9F5-E4EF","mountpoints":[null]}]},
      {"name":"sdc","path":"/dev/sdc","type":"disk","size":31266439168,"rm":true,"hotplug":true,"tran":"usb","fstype":null,"label":null,"uuid":null,"mountpoints":[null],"model":"Storage Device","vendor":"Mass    ",
       "children":[{"name":"sdc1","path":"/dev/sdc1","type":"part","size":31262244864,"rm":true,"hotplug":true,"tran":null,"fstype":"exfat","label":"CAM","uuid":"6E91-4AEE","mountpoints":["/run/media/u/CAM"]}]},
      {"name":"mmcblk0","path":"/dev/mmcblk0","type":"disk","size":64000000000,"rm":false,"hotplug":false,"tran":null,"fstype":null,"label":null,"uuid":null,"mountpoints":[null],
       "children":[{"name":"mmcblk0p1","path":"/dev/mmcblk0p1","type":"part","size":1,"rm":false,"hotplug":false,"tran":null,"fstype":"ext4","label":null,"uuid":"x","mountpoints":["/"]}]},
      {"name":"loop0","path":"/dev/loop0","type":"loop","size":100,"rm":false,"hotplug":false,"tran":null,"fstype":"vfat","label":null,"uuid":"L","mountpoints":[]}
    ]}"#;

    #[test]
    fn finds_only_removable_cards() {
        let devs = parse(SAMPLE, false).unwrap();
        assert_eq!(devs.len(), 1);
        let d = &devs[0];
        assert_eq!(d.dev, "/dev/sdc1");
        assert_eq!(d.fstype, "exfat");
        assert_eq!(d.label.as_deref(), Some("CAM"));
        assert_eq!(d.mountpoint, Some(PathBuf::from("/run/media/u/CAM")));
        assert_eq!(d.model, "Mass Storage Device");
        assert_eq!(d.card_id(), "6E91-4AEE");
    }

    #[test]
    fn loop_devices_on_request() {
        let devs = parse(SAMPLE, true).unwrap();
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[1].dev, "/dev/loop0");
        assert_eq!(devs[1].mountpoint, None);
    }
}
