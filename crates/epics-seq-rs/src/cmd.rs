//! The program registry and the shell commands (`seq_cmd.c`).

use std::sync::Arc;

use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_printf};
use epics_base_rs::runtime::stdlib::strtoull;
use epics_base_rs::runtime::task::thread_by_name;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::register_command;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome, CommandResult,
};
use parking_lot::Mutex;

use crate::program::SeqProgram;
use crate::pv::PvSystem;
use crate::pvt::ProgInstance;
use crate::qry::{pr, seq_chan_show, seq_queue_show, seq_show, seqcar};
use crate::seq::SeqThreadId;
use crate::task::seq_stop;
use crate::var::ProgramVars;

/// A registered program, whatever its variable type: what the `seq`
/// shell command looks a name up in.
pub trait RegisteredProgram: Sync {
    fn prog_name(&self) -> &'static str;

    /// [`seq`](fn@crate::seq) on this program, which needs the table for the
    /// instance's whole life.
    fn start(
        &'static self,
        macro_def: &str,
        stack_size: u32,
        db: &PvDatabase,
    ) -> Option<SeqThreadId>;
}

impl<V: ProgramVars> RegisteredProgram for SeqProgram<V> {
    fn prog_name(&self) -> &'static str {
        self.prog_name
    }

    fn start(
        &'static self,
        macro_def: &str,
        stack_size: u32,
        db: &PvDatabase,
    ) -> Option<SeqThreadId> {
        crate::seq(self, macro_def, stack_size, db)
    }
}

/// `struct sequencerProgram`.
struct SequencerProgram {
    prog: &'static dyn RegisteredProgram,
    instances: Vec<Arc<dyn ProgInstance>>,
}

/// `globals`, under `globals.lock`.
struct Globals {
    /// The latest registration first.
    programs: Vec<SequencerProgram>,
    pv_sys: Option<PvSystem>,
}

static GLOBALS: Mutex<Globals> = Mutex::new(Globals {
    programs: Vec::new(),
    pv_sys: None,
});

/// `seqRegisterSequencerProgram`: make `prog` startable by name from the
/// shell. A generated `<prog>Registrar` calls this. A second registration
/// of the same table is ignored; a new one goes first, as C prepends.
pub fn seq_register_sequencer_program(prog: &'static dyn RegisteredProgram) {
    let programs = &mut GLOBALS.lock().programs;
    if !programs.iter().any(|sp| std::ptr::addr_eq(sp.prog, prog)) {
        programs.insert(
            0,
            SequencerProgram {
                prog,
                instances: Vec::new(),
            },
        );
    }
}

/// `traverseSequencerPrograms`: `f` on each registered program and its
/// instance list, newest registration first, under the registry lock,
/// until `f` returns true; returns whether it did. C's final call with
/// null arguments is dropped, as every C traversee ignores it.
pub fn traverse_sequencer_programs(
    mut f: impl FnMut(&'static dyn RegisteredProgram, &mut Vec<Arc<dyn ProgInstance>>) -> bool,
) -> bool {
    GLOBALS
        .lock()
        .programs
        .iter_mut()
        .any(|sp| f(sp.prog, &mut sp.instances))
}

/// `createOrAttachPvSystem`: the IOC's one [`PvSystem`], created by the
/// first program to start and attached to by the rest.
pub fn create_or_attach_pv_system(db: &PvDatabase) -> PvSystem {
    let pv_sys = &mut GLOBALS.lock().pv_sys;
    match pv_sys {
        Some(sys) => {
            sys.attach();
            sys.clone()
        }
        None => pv_sys.insert(PvSystem::create(db)).clone(),
    }
}

/// `findThread`: a thread by hex id or by name.
fn find_thread(name: &[u8]) -> Option<SeqThreadId> {
    // C's `strtoul` is pointer-wide on LP64; `strtoull` keeps that on LLP64.
    let (id, end) = strtoull(name, 16);
    if end != 0 && end == name.len() {
        // `(epicsThreadId)0` is C's NULL: the callers treat it as no thread.
        return (id != 0).then_some(SeqThreadId(id));
    }
    if let Some(thread) = std::str::from_utf8(name).ok().and_then(thread_by_name) {
        return Some(SeqThreadId(thread.id()));
    }
    pr!("No such thread.\n");
    None
}

fn str_arg(args: &[ArgValue], i: usize) -> Option<&[u8]> {
    match args.get(i) {
        Some(ArgValue::String(s)) => Some(s.as_bytes()),
        _ => None,
    }
}

/// C's `ival`: 0 when absent.
fn int_arg(args: &[ArgValue], i: usize) -> i32 {
    match args.get(i) {
        Some(ArgValue::Int(v)) => *v as i32,
        _ => 0,
    }
}

/// `seqCallFunc`.
fn seq_call(args: &[ArgValue], db: &PvDatabase) {
    let Some(table) = str_arg(args, 0) else {
        pr!("No sequencer specified.\n");
        return;
    };
    let stack_size = int_arg(args, 2);
    if stack_size < 0 {
        errlog_sev_printf(
            ErrlogSevEnum::Fatal,
            "3rd argument of seq must be a positive integer",
        );
        return;
    }
    let table = table.strip_prefix(b"&").unwrap_or(table);
    let mut found = None;
    traverse_sequencer_programs(|prog, _| {
        let hit = prog.prog_name().as_bytes() == table;
        if hit {
            found = Some(prog);
        }
        hit
    });
    match found {
        Some(prog) => {
            let macro_def = str_arg(args, 1).map(String::from_utf8_lossy);
            prog.start(macro_def.as_deref().unwrap_or(""), stack_size as u32, db);
        }
        None => pr!(
            "Can't find sequencer `{}'.\n",
            String::from_utf8_lossy(table)
        ),
    }
}

/// `seqShowCallFunc`.
fn seq_show_call(args: &[ArgValue]) {
    match str_arg(args, 0) {
        None => seq_show(None),
        Some(name) => {
            if let Some(tid) = find_thread(name) {
                seq_show(Some(tid));
            }
        }
    }
}

/// What `seqQueueShowCallFunc`, `seqStopCallFunc` and `seqChanShowCallFunc`
/// share: `f` on the thread argument 0 names, else the program list.
fn with_task(args: &[ArgValue], f: impl FnOnce(SeqThreadId)) {
    match str_arg(args, 0).and_then(find_thread) {
        Some(tid) => f(tid),
        None => {
            pr!("No sequencer task specified.\n");
            seq_show(None);
        }
    }
}

/// An iocsh command whose C function returns nothing.
fn command(
    name: &str,
    args: Vec<ArgDesc>,
    f: impl Fn(&[ArgValue], &CommandContext) + Send + Sync + 'static,
) -> CommandDef {
    CommandDef::new(
        name,
        args,
        "",
        move |args: &[ArgValue], ctx: &CommandContext| -> CommandResult {
            f(args, ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `seqRegisterSequencerCommands`: the `seq`, `seqShow`, `seqChanShow`,
/// `seqQueueShow`, `seqStop` and `seqcar` shell commands. Idempotent, as
/// every generated registrar calls it: where C registers once behind
/// `firstTime`, registering again replaces each command with itself, and
/// restores them after `iocshFree`.
pub fn seq_register_sequencer_commands() {
    let s = |name| ArgDesc {
        name,
        arg_type: ArgType::String,
    };
    let int = |name| ArgDesc {
        name,
        arg_type: ArgType::Int,
    };
    let thread = || vec![s("program/threadID")];
    register_command(command(
        "seq",
        vec![
            s("program/threadID"),
            s("macro definitions"),
            int("stack size"),
        ],
        |args, ctx| seq_call(args, ctx.db()),
    ));
    register_command(command("seqShow", thread(), |args, _| seq_show_call(args)));
    register_command(command("seqQueueShow", thread(), |args, _| {
        with_task(args, |tid| seq_queue_show(Some(tid)))
    }));
    register_command(command("seqStop", thread(), |args, _| {
        with_task(args, seq_stop)
    }));
    register_command(command(
        "seqChanShow",
        vec![s("program/threadID"), s("channel")],
        |args, _| {
            let chan = str_arg(args, 1).map(String::from_utf8_lossy);
            with_task(args, |tid| seq_chan_show(Some(tid), chan.as_deref()))
        },
    ));
    register_command(command("seqcar", vec![int("verbosity")], |args, _| {
        seqcar(int_arg(args, 0))
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    use epics_base_rs::runtime::task::{
        StackSizeClass, ThreadPriority, current_thread_id, spawn_dedicated_thread,
    };

    use crate::qry::capture;

    struct Named(&'static str);

    impl RegisteredProgram for Named {
        fn prog_name(&self) -> &'static str {
            self.0
        }

        fn start(&'static self, _: &str, _: u32, _: &PvDatabase) -> Option<SeqThreadId> {
            None
        }
    }

    /// A program whose `start` records its macros and stack size.
    struct Recorder(&'static str, Mutex<Vec<(String, u32)>>);

    impl Recorder {
        const fn new(name: &'static str) -> Self {
            Recorder(name, Mutex::new(Vec::new()))
        }
    }

    impl RegisteredProgram for Recorder {
        fn prog_name(&self) -> &'static str {
            self.0
        }

        fn start(
            &'static self,
            macro_def: &str,
            stack_size: u32,
            _: &PvDatabase,
        ) -> Option<SeqThreadId> {
            self.1.lock().push((macro_def.to_owned(), stack_size));
            None
        }
    }

    /// `seq` with `args`, and what it prints.
    fn seq_out(args: &[ArgValue]) -> String {
        capture(b"", || seq_call(args, &PvDatabase::new()))
    }

    fn s(arg: &str) -> ArgValue {
        ArgValue::String(arg.into())
    }

    #[test]
    fn seq_starts_the_named_program_with_or_without_one_ampersand() {
        static P: Recorder = Recorder::new("cmdTestSeq");
        seq_register_sequencer_program(&P);
        assert_eq!(
            seq_out(&[s("&cmdTestSeq"), s("P=x:"), ArgValue::Int(4000)]),
            ""
        );
        assert_eq!(
            seq_out(&[s("cmdTestSeq"), ArgValue::Missing, ArgValue::Missing]),
            ""
        );
        assert_eq!(
            seq_out(&[s("&&cmdTestSeq"), ArgValue::Missing, ArgValue::Missing]),
            "Can't find sequencer `&cmdTestSeq'.\n"
        );
        assert_eq!(*P.1.lock(), [("P=x:".to_owned(), 4000), (String::new(), 0)]);
    }

    #[test]
    fn seq_names_a_program_nobody_registered() {
        assert_eq!(
            seq_out(&[s("&cmdTestUnknown"), ArgValue::Missing, ArgValue::Missing]),
            "Can't find sequencer `cmdTestUnknown'.\n"
        );
    }

    #[test]
    fn seq_without_a_program_prints_no_sequencer_specified() {
        assert_eq!(
            seq_out(&[ArgValue::Missing, ArgValue::Missing, ArgValue::Missing]),
            "No sequencer specified.\n"
        );
    }

    #[test]
    fn seq_with_a_negative_stack_size_is_fatal_and_starts_nothing() {
        use epics_base_rs::runtime::log::{
            errlog_add_listener, errlog_flush, errlog_remove_listener,
        };

        static P: Recorder = Recorder::new("cmdTestStack");
        seq_register_sequencer_program(&P);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let id = errlog_add_listener(move |m: &str| sink.lock().push(m.to_owned()));
        let out = seq_out(&[s("cmdTestStack"), ArgValue::Missing, ArgValue::Int(-1)]);
        errlog_flush();
        errlog_remove_listener(id);
        assert_eq!(out, "");
        assert!(P.1.lock().is_empty());
        let line = "sevr=fatal 3rd argument of seq must be a positive integer";
        let seen = seen.lock();
        assert!(seen.iter().any(|m| m.trim_end() == line), "{seen:?}");
    }

    fn names(prefix: &str) -> Vec<&'static str> {
        let mut names = Vec::new();
        traverse_sequencer_programs(|prog, _| {
            names.push(prog.prog_name());
            false
        });
        names.retain(|n| n.starts_with(prefix));
        names
    }

    #[test]
    fn registration_prepends_and_ignores_a_second_registration() {
        static A: Named = Named("cmdTestA");
        static B: Named = Named("cmdTestB");
        seq_register_sequencer_program(&A);
        seq_register_sequencer_program(&B);
        seq_register_sequencer_program(&A);
        assert_eq!(names("cmdTest"), ["cmdTestB", "cmdTestA"]);
    }

    #[test]
    fn traversal_stops_at_the_first_true() {
        static C: Named = Named("stopTestC");
        static D: Named = Named("stopTestD");
        seq_register_sequencer_program(&C);
        seq_register_sequencer_program(&D);
        let mut seen = Vec::new();
        let stopped = traverse_sequencer_programs(|prog, _| {
            seen.push(prog.prog_name());
            prog.prog_name() == "stopTestD"
        });
        assert!(stopped);
        assert!(!seen.contains(&"stopTestC"));
    }

    fn find(name: &[u8]) -> (Option<SeqThreadId>, String) {
        let mut tid = None;
        let out = capture(b"", || tid = find_thread(name));
        (tid, out)
    }

    #[test]
    fn a_thread_id_parses_as_hex_with_or_without_0x() {
        assert_eq!(
            find(b"0x60383c9a0200"),
            (Some(SeqThreadId(0x60383c9a0200)), String::new())
        );
        assert_eq!(find(b"1a"), (Some(SeqThreadId(0x1a)), String::new()));
    }

    #[test]
    fn a_zero_id_is_no_thread_and_prints_nothing() {
        assert_eq!(find(b"0"), (None, String::new()));
    }

    #[test]
    fn an_unknown_name_prints_no_such_thread() {
        assert_eq!(find(b""), (None, "No such thread.\n".to_owned()));
        assert_eq!(find(b"0x1g"), (None, "No such thread.\n".to_owned()));
    }

    #[test]
    fn a_missing_task_argument_prints_the_usage_and_the_program_list() {
        let out = capture(b"", || with_task(&[ArgValue::Missing], |_| unreachable!()));
        assert!(out.starts_with("No sequencer task specified.\n"), "{out}");
    }

    #[test]
    fn a_thread_name_resolves_to_its_epics_id() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let handle = spawn_dedicated_thread(
            "cmdTestThread".into(),
            ThreadPriority::Medium,
            StackSizeClass::Small,
            move || {
                tx.send(current_thread_id()).unwrap();
                let _ = done_rx.recv();
            },
        )
        .unwrap();
        let id = rx.recv().unwrap();
        assert_eq!(
            find(b"cmdTestThread"),
            (Some(SeqThreadId(id)), String::new())
        );
        drop(done_tx);
        handle.join().unwrap();
    }
}
