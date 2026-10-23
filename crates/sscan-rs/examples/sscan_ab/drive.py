"""Drive one IOC through a 1-D (S1) then 2-D (S2) scan; record monitor
streams of the scan fields and the final arrays as JSON on stdout."""
import json, os, sys, time
port = sys.argv[1]
os.environ.update(EPICS_CA_AUTO_ADDR_LIST='NO', EPICS_CA_ADDR_LIST='127.0.0.1',
                  EPICS_CA_SERVER_PORT=port)
import epics
SCALARS = ['CPT', 'FAZE', 'DATA', 'BUSY', 'EXSC', 'XSC', 'P1DV', 'P1CV', 'R1CV',
           'D01CV', 'SMSG', 'ALRT', 'DSTATE', 'NPTS', 'P1NV', 'D01NV', 'T1NV']
ARRAYS = ['P1RA', 'D01DA', 'P1CA', 'D01CA']
out = {}
def run(scan, tag, others=()):
    events = {}
    pvs = []
    for rec in (scan,) + tuple(others):
        for f in SCALARS:
            name = f'{rec}.{f}'
            events[name] = []
            def cb(pvname=None, value=None, char_value=None, **kw):
                events[pvname].append(char_value if isinstance(value, str) else
                                      (round(value, 9) if isinstance(value, float) else value))
            pv = epics.PV(name, callback=cb, form='native', auto_monitor=True)
            pvs.append(pv)
    for pv in pvs:
        assert pv.wait_for_connection(5), pv.pvname
    time.sleep(1.0)
    for k in events: events[k] = events[k][-1:]   # keep the initial value only
    epics.caput(f'{scan}.EXSC', 1)
    t0 = time.time()
    while time.time() - t0 < 30:
        time.sleep(0.2)
        if epics.caget(f'{scan}.BUSY') == 0 and epics.caget(f'{scan}.DATA') == 1:
            break
    time.sleep(1.0)
    final = {}
    for rec in (scan,) + tuple(others):
        npts = int(epics.caget(f'{rec}.NPTS'))
        for a in ARRAYS:
            v = epics.caget(f'{rec}.{a}', count=npts)
            final[f'{rec}.{a}'] = [round(float(x), 6) for x in v]
        for f in SCALARS:
            v = epics.caget(f'{rec}.{f}', as_string=f == 'SMSG')
            final[f'{rec}.{f}'] = round(v, 9) if isinstance(v, float) else v
    for pv in pvs: pv.clear_callbacks(); pv.disconnect()
    out[tag] = {'events': events, 'final': final}
run('S1', '1d')
run('S2', '2d', others=('S1',))
json.dump(out, sys.stdout, indent=1, default=str)
