#!../../target/debug/examples/scope_ioc
#============================================================
# st.cmd — Scope Simulator IOC startup script
#
# Port of EPICS testAsynPortDriver IOC.
#
# Usage:
#   cargo run --example scope_ioc --features epics -- ioc/st.cmd
#============================================================

# Environment
epicsEnvSet("PREFIX", "SCOPE:")
epicsEnvSet("PORT",   "scopeSim")

# Create the scope simulator driver
# scopeSimulatorConfig(portName)
scopeSimulatorConfig("$(PORT)")

# Load the scope database
dbLoadRecords("$(ASYN)/Db/scopeSimulator.db", "P=$(PREFIX),R=$(PORT):")

# iocInit is called automatically by IocApplication after this script completes.
iocInit()

# After init, the interactive iocsh shell starts.
#
# Example interactive commands:
#   dbl                                         # List all PVs
#   dbpf SCOPE:scopeSim:Run 1                   # Start acquisition
#   dbgf SCOPE:scopeSim:MeanValue_RBV           # Read mean value
#   dbpf SCOPE:scopeSim:NoiseAmplitude 0.2      # Set noise
#   dbpf SCOPE:scopeSim:VertGainSelect 3        # x10 gain
#   dbpf SCOPE:scopeSim:Run 0                   # Stop
#   scopeSimulatorReport                        # Show driver status
