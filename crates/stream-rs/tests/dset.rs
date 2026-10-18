//! Every `dev*Stream.c` dset's first entry is `streamReport`, which
//! devStream.h:110 defines as NULL, so `dbDumpDevice` prints its `report()`
//! as `(nil)`.

#[test]
fn report_is_null() {
    for (rtype, devs) in stream_rs::dev::devices() {
        for dev in devs {
            assert!(!dev.dset.funcs.contains(&0), "{rtype}: {dev:?}");
        }
    }
}
