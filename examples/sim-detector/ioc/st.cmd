#!../../target/debug/sim_ioc
#============================================================
# st.cmd — SimDetector IOC startup script
#
# Matches C++ ADSimDetector IOC startup structure with
# commonPlugins.cmd include for plugin configuration.
#
# Usage:
#   cargo run --bin sim_ioc --features ioc -- ioc/st.cmd
#============================================================

# Environment
epicsEnvSet("PREFIX", "SIM1:")
epicsEnvSet("CAM",    "cam1:")
epicsEnvSet("PORT",   "SIM1")
epicsEnvSet("QSIZE",  "20")
epicsEnvSet("XSIZE",  "1024")
epicsEnvSet("YSIZE",  "1024")
epicsEnvSet("NCHANS", "2048")
epicsEnvSet("CBUFFS", "500")
epicsEnvSet("NELEMENTS", "1048576")
epicsEnvSet("EPICS_DB_INCLUDE_PATH", "$(ADCORE)/db")

# Autosave configuration
set_requestfile_path("$(ADSIMDETECTOR)/db")
set_requestfile_path("$(ADCORE)/db")
set_savefile_path("$(ADSIMDETECTOR)/ioc/autosave")
save_restoreSet_status_prefix("$(PREFIX)")
set_pass0_restoreFile("simDetector_settings.req", "P=$(PREFIX),R=$(CAM)")
set_pass1_restoreFile("simDetector_settings.req", "P=$(PREFIX),R=$(CAM)")

# Create the SimDetector driver
simDetectorConfig("$(PORT)", 1024, 1024, 50000000)

# Load the detector database
dbLoadRecords("$(ADSIMDETECTOR)/db/simDetector.template", "P=$(PREFIX),R=$(CAM),PORT=$(PORT),ADDR=0,TIMEOUT=1")

# Load all common plugins (includes image1 StdArrays)
< $(ADCORE)/ioc/commonPlugins.cmd

# Autosave monitor sets (after all records are loaded)
create_monitor_set("simDetector_settings.req", 5, "P=$(PREFIX),R=$(CAM)")

# iocInit is called automatically by IocApplication after this script completes.
#
# After init, the interactive iocsh shell starts.
#
# Example interactive commands:
#   dbl                                # List all PVs
#   dbpf SIM1:cam1:Acquire 1           # Start acquisition
#   dbgf SIM1:cam1:ArrayCounter_RBV    # Read frame counter
#   simDetectorReport                  # Show detector status
