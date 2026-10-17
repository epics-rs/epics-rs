#!../../target/debug/sim-ioc
#============================================================
# st.cmd — SimDetector IOC startup script
#
# Matches C++ ADSimDetector IOC startup structure with
# commonPlugins.cmd include for plugin configuration.
#
# Usage:
#   cargo run --bin sim-ioc --features ioc -- ioc/st.cmd
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

# Create the SimDetector driver
simDetectorConfig("$(PORT)", 1024, 1024, 1, 0, 50000000)

# Load the detector database
dbLoadRecords("$(ADSIMDETECTOR)/db/simDetector.template", "P=$(PREFIX),R=$(CAM),PORT=$(PORT),ADDR=0,TIMEOUT=1")

# image1, which commonPlugins.cmd leaves to the st.cmd as C's does
NDStdArraysConfigure("IMAGE1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStdArrays.template", "P=$(PREFIX),R=image1:,PORT=IMAGE1,NDARRAY_PORT=$(PORT),TYPE=$(TYPE=Int8),FTVL=$(FTVL=UCHAR),NELEMENTS=$(NELEMENTS)")
< $(ADCORE)/iocBoot/commonPlugins.cmd

# Autosave configuration, after commonPlugins.cmd, whose C autosave block
# sets the save path to ./autosave
set_requestfile_path("$(ADSIMDETECTOR)/db")
set_savefile_path("$(ADSIMDETECTOR)/ioc/autosave")
set_pass0_restoreFile("simDetector_settings.req", "P=$(PREFIX),R=$(CAM)")
set_pass1_restoreFile("simDetector_settings.req", "P=$(PREFIX),R=$(CAM)")

# Autosave monitor sets (after all records are loaded)
create_monitor_set("simDetector_settings.req", 5, "P=$(PREFIX),R=$(CAM)")

iocInit()

# After init, the interactive iocsh shell starts.
#
# Example interactive commands:
#   dbl                                # List all PVs
#   dbpf SIM1:cam1:Acquire 1           # Start acquisition
#   dbgf SIM1:cam1:ArrayCounter_RBV    # Read frame counter
#   simDetectorReport                  # Show detector status
