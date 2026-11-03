#============================================================
# st.cmd — QSRV Demo IOC startup script
#
# Serves ai/ao/bi records + a group PV over pvAccess (QSRV).
#
# Usage:
#   cargo run --release -p qsrv-ioc --features ioc -- ioc/st.cmd
#
# Test:
#   pvget  DEMO:AI                # NTScalar — simulated temperature
#   pvget  DEMO:AO                # NTScalar — writable setpoint
#   pvget  DEMO:BI                # NTEnum  — beam status (Off/On)
#   pvget  DEMO:GROUP             # Group PV — composite structure
#   pvmonitor DEMO:GROUP          # Live group updates
#   pvget  DEMO:table             # NTTable — five typed columns
#   pvget  DEMO:ntndarray         # NTNDArray — 3x2 int64 image
#   pvput  DEMO:AO 42.5           # Write setpoint
#============================================================

# Environment
epicsEnvSet("P", "DEMO:")
epicsEnvSet("QSRV_IOC", "$(QSRV_IOC)")

# Load records
dbLoadRecords("$(QSRV_IOC)/db/qsrv_demo.db", "P=$(P)")
dbLoadRecords("$(QSRV_IOC)/db/nt_groups.db", "device=$(P)")

# Load group PV definitions (pvxs/QSRV-compatible JSON)
qsrvGroupLoadConfig("$(QSRV_IOC)/db/group.json")

# Start the IOC
iocInit()

# After init, the iocsh prompt is available:
#   dbl                           # List all PVs
#   dbgf DEMO:AI                  # Read AI value
#   dbpf DEMO:AO 42.5             # Set AO value
#   dbpf DEMO:BI 1                # Set BI to "On"
