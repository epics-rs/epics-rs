//! An mca pass that ends still acquiring withholds `recGblFwdLink`, so it
//! consumes RPRO itself and runs again (`mcaRecord.c:827-839`): a `dbpf` STOP
//! made while the status read is owed goes out on the pass after the read's
//! callback, not when the acquisition ends.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::{CallbackPriority, sleep, test_block_on};
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, ReadStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;
use mca_rs::record::{McaCommand, McaDset, McaRead, McaRecord, McaStatus, Nack, Sent};

static PORT: [DevSup; 1] = [DevSup {
    name: "devMcaPort",
    choice: "Port",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 3, 4, 5], true),
}];

type Log = Arc<Mutex<Vec<&'static str>>>;
type Owed = Arc<Mutex<Vec<(DeviceDone, CallbackPriority)>>>;

/// devMcaAsyn on a CANBLOCK port that is always acquiring: each read queues
/// its callback on [`Owed`].
struct Port {
    reentry: Option<(DeviceDone, CallbackPriority)>,
    owed: Owed,
    log: Log,
}

impl McaDset for Port {
    fn send_msg(&mut self, command: McaCommand) -> Result<(), Nack> {
        if matches!(command, McaCommand::StopAcquire) {
            self.log.lock().unwrap().push("stop");
        }
        Ok(())
    }
    fn send_read(&mut self, read: McaRead) -> Result<Sent, Nack> {
        self.log.lock().unwrap().push(match read {
            McaRead::Status => "send_status",
            McaRead::Data => "send_data",
        });
        self.owed
            .lock()
            .unwrap()
            .push(self.reentry.clone().expect("each read re-enters"));
        Ok(Sent::Queued)
    }
    fn read_status(&mut self, _mca: &McaRecord) -> Result<McaStatus, Nack> {
        Ok(McaStatus {
            acquiring: true,
            ..McaStatus::default()
        })
    }
    fn read_array(&mut self, _mca: &mut McaRecord) -> Result<(), Nack> {
        Ok(())
    }
}

struct DevPort(Port);

impl DeviceSupport for DevPort {
    fn dtyp(&self) -> &str {
        "Port"
    }
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        Ok(DeviceInitOutcome::ok())
    }
    fn read_begin(
        &mut self,
        record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        self.0.reentry = Some((done.device_done(), done.priority()));
        let mca = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<McaRecord>())
            .unwrap();
        Ok(mca.device_read_begin(&mut self.0, done))
    }
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[test]
fn a_stop_dbpf_during_the_status_read_goes_out_after_it() {
    test_block_on(async {
        let log: Log = Arc::default();
        let owed: Owed = Arc::default();
        let (l, o) = (Arc::clone(&log), Arc::clone(&owed));
        let (name, factory) = mca_rs::mca_record_factory();
        let (db, _) = IocBuilder::new()
            .register_record_type(name, factory)
            .register_dynamic_device_support(
                &[("mca", &PORT)],
                move |ctx: &DeviceSupportContext| {
                    (ctx.dtyp == "Port").then(|| {
                        Box::new(DevPort(Port {
                            reentry: None,
                            owed: Arc::clone(&o),
                            log: Arc::clone(&l),
                        })) as Box<dyn DeviceSupport>
                    })
                },
            )
            .db_string(
                r#"record(mca, "M") { field(DTYP, "Port") field(NMAX, "4") field(NUSE, "4") }"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        db.ioc_init().await;
        log.lock().unwrap().clear();

        db.put_record_field_from_ca("M", "PROC", EpicsValue::String("1".into()))
            .await
            .unwrap();
        db.put_record_field_from_ca_no_notify("M", "STOP", EpicsValue::String("1".into()))
            .await
            .unwrap();
        let rpro = num(&db, "M.RPRO");
        // The status read's callback: its pass ends still acquiring.
        let (done, band) = owed.lock().unwrap().remove(0);
        std::thread::spawn(move || done.reprocess(band).unwrap())
            .join()
            .unwrap();
        for _ in 0..400 {
            if log.lock().unwrap().len() >= 3 {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            (rpro, log.lock().unwrap().clone(), num(&db, "M.RPRO")),
            (1.0, vec!["send_status", "stop", "send_status"], 0.0),
            "RPRO after the dbpf; the dset calls and RPRO after the callback's pass"
        );
    });
}
