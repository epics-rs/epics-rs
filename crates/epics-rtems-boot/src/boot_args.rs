//! The boot command line — the target's `st.cmd`.
//!
//! # What this replaces
//!
//! A C IOC on RTEMS reaches every `EPICS_*` variable through iocsh. Base's
//! `POSIX_Init` takes a startup-script path from the BOOTP/DHCP command line
//! into `argv[1]`
//! (`$EPICS_BASE/modules/libcom/RTEMS/posix/rtems_init.c:103,256,1165` at
//! `R7.0.10` — `bootp_cmdline_init`, `argv[1] = rtems_bsdnet_bootp_cmdline`,
//! `epicsEnvSet("IOC_STARTUP_SCRIPT", argv[1])`), hands it to `main` at
//! `:1184`, and the generated IOC main runs it through `iocsh`. Every
//! `epicsEnvSet` line in that script is a `setenv` before `iocInit`.
//!
//! This port drops the startup script and the remote filesystems that carry it,
//! so the *carrier* has to be the
//! command line itself rather than a path to a file on a filesystem the image
//! does not have. What must not differ is the observable: a site sets
//! `EPICS_CA_ADDR_LIST`, `EPICS_CAS_INTF_ADDR_LIST` or any other tuning
//! variable at boot and the IOC honours it. Before this module the target
//! honoured none of them and said nothing — `csrc/rtems_init.c` called `setenv`
//! zero times and passed a one-element `argv`, so the compiled-in defaults were
//! the only values an image could ever have.
//!
//! # The command language
//!
//! Both target IOCs already document argv as their `st.cmd`
//! (`realtime-pva-ioc.rs`: *"argv **is** st.cmd and this is the whole command
//! language"*). This module adds the one verb that language was missing:
//!
//! * `NAME=VALUE` — `epicsEnvSet NAME VALUE`. `NAME` must be a POSIX
//!   environment name (`[A-Za-z_][A-Za-z0-9_]*`); that rule, not position, is
//!   what tells an assignment from a file name, so `/db/a=1.db` is a path and
//!   `EPICS_CA_ADDR_LIST=10.0.2.2` is an assignment wherever either appears.
//! * anything else — a load argument, passed through untouched in its original
//!   order for the caller's own `dbLoadRecords` / `dbLoadGroup` split.
//!
//! **Every assignment is applied before any load argument is looked at.** That
//! is stricter than iocsh, where an `epicsEnvSet` after a `dbLoadRecords` is
//! legal and affects only what follows it. The uniform rule is deliberate: it
//! makes "the environment is complete before the first record is parsed and
//! before any server reads its configuration" true by construction, rather than
//! a property of how the operator happened to order one line against another.
//!
//! # Where the line comes from
//!
//! `csrc/rtems_init.c` builds it, from the same two sources base uses and in
//! the same precedence: the compile-time default `EPICS_RTEMS_CMDLINE` (base's
//! `bootp_cmdline_init`, `:103`) overridden by the DHCP option `rtems_cmdline`
//! (base reads it as `new_rtems_cmdline` from the dhcpcd environment, `:762`).
//! The shim splits it on whitespace into `argv[1..]`; this module gives the
//! tokens their meaning.

/// Whether `name` is a POSIX environment variable name.
///
/// This is the whole discriminator between an assignment and a load argument,
/// so it is deliberately the `putenv` rule and nothing looser: an empty name, a
/// leading digit, a slash or a dot all mean "not an assignment", which is what
/// keeps a path containing `=` a path.
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Split a boot command line into its environment assignments and its load
/// arguments, in order.
///
/// Pure, so the rule that decides which token is which is testable off-target;
/// [`apply_boot_env`] is the only thing that touches the process environment.
///
/// A token spelled `NAME=` assigns the empty string, matching `epicsEnvSet
/// NAME ""` — that is how a site turns `EPICS_CA_AUTO_ADDR_LIST` off by
/// clearing a list, so it must not be read as "unset" or dropped.
pub fn split_boot_args<I, S>(args: I) -> (Vec<(String, String)>, Vec<String>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut env = Vec::new();
    let mut load = Vec::new();
    for arg in args {
        let arg = arg.as_ref();
        match arg.split_once('=') {
            Some((name, value)) if is_env_name(name) => {
                env.push((name.to_string(), value.to_string()));
            }
            _ => load.push(arg.to_string()),
        }
    }
    (env, load)
}

/// Apply a boot command line's environment assignments and return its load
/// arguments.
///
/// The one owner of the boot line's effect on the process environment: both
/// target IOCs call this and neither writes `set_var` for a boot argument
/// itself, so the assignment rule cannot come to differ between them.
///
/// Assignments are applied in order and an assignment *always wins* over
/// whatever the image was built with — a compiled-in default the operator
/// cannot override is the defect this exists to close. Each one is echoed to
/// stdout, because the console is this target's only report and a variable that
/// silently failed to take effect is exactly the failure that cost the round
/// this row.
///
/// # Safety
///
/// Calls [`std::env::set_var`]. The caller must guarantee that no other thread
/// exists yet — that is, that this runs on the entry thread before anything has
/// been spawned. Both target IOCs call it as their first statement after
/// `link_anchor()` for that reason.
pub unsafe fn apply_boot_env<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let (env, load) = split_boot_args(args);
    for (name, value) in env {
        println!("rtems-boot: epicsEnvSet {name} \"{value}\"");
        // SAFETY: the caller's contract above — entry thread, no other thread
        // started yet, so there is no concurrent reader or writer.
        unsafe { std::env::set_var(&name, &value) };
    }
    load
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_assignment_is_told_from_a_path_by_the_name_rule() {
        let (env, load) = split_boot_args([
            "EPICS_CA_ADDR_LIST=10.0.2.2",
            "/epics/db/site.db",
            "_PRIVATE=1",
            "groups.json",
        ]);
        assert_eq!(
            env,
            vec![
                ("EPICS_CA_ADDR_LIST".to_string(), "10.0.2.2".to_string()),
                ("_PRIVATE".to_string(), "1".to_string()),
            ]
        );
        assert_eq!(load, vec!["/epics/db/site.db", "groups.json"]);
    }

    #[test]
    fn a_path_containing_an_equals_sign_stays_a_path() {
        // The reason the discriminator is the POSIX name rule rather than
        // "contains '='": every one of these has an '=' and none is an
        // assignment.
        for path in ["/db/a=1.db", "./a=b.db", "a-b=c", "9UP=1", "=v", "a b=c"] {
            let (env, load) = split_boot_args([path]);
            assert!(env.is_empty(), "{path} was read as an assignment");
            assert_eq!(load, vec![path.to_string()]);
        }
    }

    #[test]
    fn an_empty_value_assigns_rather_than_unsets() {
        // `EPICS_CA_ADDR_LIST=` is how a site clears a compiled-in list; if
        // this were dropped the compiled-in value would survive, which is the
        // defect this module closes.
        let (env, load) = split_boot_args(["EPICS_CA_ADDR_LIST="]);
        assert_eq!(env, vec![("EPICS_CA_ADDR_LIST".to_string(), String::new())]);
        assert!(load.is_empty());
    }

    #[test]
    fn a_value_may_contain_further_equals_signs() {
        // `split_once`, not `split`: an `=` inside a value is data. macro text
        // (`dbLoadRecords`'s second argument) is the case that needs it.
        let (env, _) = split_boot_args(["MACROS=P=X:,R=1"]);
        assert_eq!(env, vec![("MACROS".to_string(), "P=X:,R=1".to_string())]);
    }

    #[test]
    fn load_arguments_keep_their_order_across_interleaved_assignments() {
        // The load list is the `dbLoadRecords` order, and `dbLoadRecords` order
        // decides which duplicate record definition wins. Assignments must not
        // perturb it.
        let (_, load) = split_boot_args(["a.db", "P=1", "b.db", "Q=2", "c.json"]);
        assert_eq!(load, vec!["a.db", "b.db", "c.json"]);
    }

    #[test]
    fn a_bare_name_with_no_equals_is_a_load_argument() {
        let (env, load) = split_boot_args(["EPICS_CA_ADDR_LIST"]);
        assert!(env.is_empty());
        assert_eq!(load, vec!["EPICS_CA_ADDR_LIST"]);
    }
}
