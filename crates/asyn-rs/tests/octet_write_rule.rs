//! A C driver overrides the virtual `writeOctet` to act on a client write of
//! a reason — asynNDArrayDriver::writeOctet reloads the attributes file after
//! storing `NDAttributesFile`. `PortDriverBase::set_write_octet_rule` is that
//! override; it runs from the `PortDriver::write_octet` default and from any
//! override that chains to `PortDriverBase::write_octet`.

use std::sync::{Arc, Mutex};

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::user::AsynUser;

struct Det {
    base: PortDriverBase,
}

impl PortDriver for Det {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

/// A derived driver that overrides `write_octet` and chains to its base.
struct Derived {
    base: PortDriverBase,
    own: usize,
}

impl PortDriver for Derived {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        if user.reason == self.own {
            return Ok(data.len());
        }
        self.base.write_octet(user, data)
    }
}

/// What the rule saw: the stored value of `file` at the time it ran.
type Seen = Arc<Mutex<Vec<Vec<u8>>>>;

fn base(name: &str, seen: Seen) -> (PortDriverBase, usize, usize) {
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    let file = base.create_param("FILE", ParamType::Octet).unwrap();
    let other = base.create_param("OTHER", ParamType::Octet).unwrap();
    base.set_write_octet_rule(
        file,
        Arc::new(move |base: &mut PortDriverBase, addr, _data: &[u8]| {
            seen.lock()
                .unwrap()
                .push(base.get_string_param(file, addr).unwrap().to_vec());
        }),
    );
    (base, file, other)
}

#[test]
fn a_client_write_runs_the_rule_after_the_store() {
    let seen = Seen::default();
    let (base, file, other) = base("OCTRULE", seen.clone());
    let mut det = Det { base };

    det.base.set_string_param(file, 0, b"direct").unwrap();
    assert!(seen.lock().unwrap().is_empty(), "a store is not a write");
    det.write_octet(&mut AsynUser::new(other), b"x").unwrap();
    assert!(seen.lock().unwrap().is_empty(), "another reason");

    det.write_octet(&mut AsynUser::new(file), b"attrs.xml")
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![b"attrs.xml".to_vec()]);
}

#[test]
fn an_override_chains_to_the_rule() {
    let seen = Seen::default();
    let (mut base, file, _) = base("OCTRULEDERIVED", seen.clone());
    let own = base.create_param("OWN", ParamType::Octet).unwrap();
    let mut det = Derived { base, own };

    det.write_octet(&mut AsynUser::new(own), b"mine").unwrap();
    det.write_octet(&mut AsynUser::new(file), b"attrs.xml")
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![b"attrs.xml".to_vec()]);
}
