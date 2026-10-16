//! BIND-style zone snippet generator.
//!
//! Operators who don't run dynamic DNS update can drop the output of
//! `ZoneSnippet::render()` straight into a zone file and reload.

#![cfg(feature = "discovery")]

use std::fmt::Write;

/// Builder for a DNS-SD zone snippet.
pub struct ZoneSnippet {
    instances: Vec<Instance>,
}

struct Instance {
    name: String,
    host: String,
    port: u16,
    txt: Vec<(String, String)>,
}

impl ZoneSnippet {
    pub fn new() -> Self {
        Self {
            instances: Vec::new(),
        }
    }

    /// Register an IOC instance. `name` is the unique service-instance
    /// name (e.g. "motor-ioc"), `host` is a hostname A record on the
    /// same zone (e.g. "motor-host"), `port` is the UDP search port a client
    /// sends SEARCH to: a discovering client adds the SRV `host:port` to its
    /// SEARCH destinations. It equals the TCP port in the default setup, and
    /// differs only when the server was given distinct ones. The snippet is
    /// written offline, so the operator supplies the number.
    pub fn instance(mut self, name: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        self.instances.push(Instance {
            name: name.into(),
            host: host.into(),
            port,
            txt: Vec::new(),
        });
        self
    }

    /// Attach a key=value pair to the most recently added instance's
    /// TXT record. Use for metadata like `version`, `asg`, `owner`.
    pub fn txt(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if let Some(last) = self.instances.last_mut() {
            last.txt.push((key.into(), value.into()));
        }
        self
    }

    /// Render the snippet for inclusion in a BIND zone file. Caller is
    /// expected to add `$ORIGIN <zone>.` at the top if not already
    /// implicit from the file's location.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "; epics-ca-rs DNS-SD snippet");
        let _ = writeln!(out, "; Append to your zone file and `rndc reload`.\n");

        // PTR records
        for inst in &self.instances {
            let _ = writeln!(
                out,
                "_epics-ca._tcp                  PTR    {name}._epics-ca._tcp",
                name = inst.name
            );
        }
        let _ = writeln!(out);

        // SRV + TXT per instance
        for inst in &self.instances {
            let _ = writeln!(
                out,
                "{name}._epics-ca._tcp        SRV    0 0 {port} {host}",
                name = inst.name,
                port = inst.port,
                host = inst.host
            );
            if !inst.txt.is_empty() {
                let mut txt_line = format!("{}._epics-ca._tcp        TXT    ", inst.name);
                for (k, v) in &inst.txt {
                    txt_line.push_str(&format!("\"{k}={v}\" "));
                }
                let _ = writeln!(out, "{}", txt_line.trim_end());
            }
        }

        out
    }
}

impl Default for ZoneSnippet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_includes_all_records() {
        let s = ZoneSnippet::new()
            .instance("motor-ioc", "motor-host", 5064)
            .txt("version", "4.13")
            .txt("asg", "BEAM")
            .instance("bpm-ioc", "bpm-host", 5064)
            .render();
        assert!(s.contains("PTR    motor-ioc._epics-ca._tcp"));
        assert!(s.contains("PTR    bpm-ioc._epics-ca._tcp"));
        assert!(s.contains("motor-ioc._epics-ca._tcp        SRV    0 0 5064 motor-host"));
        assert!(s.contains("\"version=4.13\""));
        assert!(s.contains("\"asg=BEAM\""));
        assert!(s.contains("bpm-ioc._epics-ca._tcp        SRV    0 0 5064 bpm-host"));
    }

    #[test]
    fn empty_snippet_still_has_header() {
        let s = ZoneSnippet::new().render();
        assert!(s.contains("epics-ca-rs DNS-SD snippet"));
    }
}
