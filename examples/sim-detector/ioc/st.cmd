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
epicsEnvSet("EPICS_DB_INCLUDE_PATH", "$(ADCORE)/db")

# Create the SimDetector driver
simDetectorConfig("$(PORT)", 1024, 1024, 50000000)

# Load the detector database
dbLoadRecords("$(ADSIMDETECTOR)/db/simDetector.template", "P=$(PREFIX),R=$(CAM),PORT=$(PORT),DTYP=asynSimDetector")

# StdArrays plugin (image data for clients)
NDStdArraysConfigure("IMAGE1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("$(ADCORE)/db/NDStdArrays.template", "P=$(PREFIX),R=image1:,PORT=IMAGE1,DTYP=asynIMAGE1,NDARRAY_PORT=$(PORT),FTVL=UCHAR,NELEMENTS=65536")

# Load all common plugins
< $(ADSIMDETECTOR)/ioc/commonPlugins.cmd

# iocInit is called automatically by IocApplication after this script completes.
#
# After init, the interactive iocsh shell starts.
#
# Example interactive commands:
#   dbl                                # List all PVs
#   dbpf SIM1:cam1:Acquire 1           # Start acquisition
#   dbgf SIM1:cam1:ArrayCounter_RBV    # Read frame counter
#   simDetectorReport                  # Show detector status
