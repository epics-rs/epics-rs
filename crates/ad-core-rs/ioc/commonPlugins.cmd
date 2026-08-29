# commonPlugins.cmd — Common plugin configuration for areaDetector IOCs
#
# Mirrors the C EPICS ADCore/iocBoot/EXAMPLE_commonPlugins.cmd structure.
# Loaded from st.cmd via: < commonPlugins.cmd
#
# Required macros (set before loading):
#   $(PREFIX)    - PV prefix
#   $(PORT)      - Detector port name
#   $(QSIZE)     - Queue size (default 20)
#   $(XSIZE)     - Max image width
#   $(YSIZE)     - Max image height
#   $(NCHANS)    - Max time series points
#   $(CBUFFS)    - Circular buffer frame count (default 500)
#   $(NELEMENTS) - Max waveform elements for image data
#   $(FTVL)      - Waveform field type (default: UCHAR)
#   $(TYPE)      - Array interface type (default: Int8)

# ===== StdArrays plugin (image data for clients) =====

NDStdArraysConfigure("IMAGE1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStdArrays.template", "P=$(PREFIX),R=image1:,PORT=IMAGE1,NDARRAY_PORT=$(PORT),TYPE=$(TYPE=Int8),FTVL=$(FTVL=UCHAR),NELEMENTS=$(NELEMENTS)")

# ===== File saving plugins =====

NDFileNetCDFConfigure("FileNetCDF1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDFileNetCDF.template", "P=$(PREFIX),R=netCDF1:,PORT=FileNetCDF1,NDARRAY_PORT=$(PORT)")

NDFileTIFFConfigure("FileTIFF1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDFileTIFF.template", "P=$(PREFIX),R=TIFF1:,PORT=FileTIFF1,NDARRAY_PORT=$(PORT)")

NDFileJPEGConfigure("FileJPEG1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDFileJPEG.template", "P=$(PREFIX),R=JPEG1:,PORT=FileJPEG1,NDARRAY_PORT=$(PORT)")

NDFileNexusConfigure("FileNexus1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDFileNexus.template", "P=$(PREFIX),R=Nexus1:,PORT=FileNexus1,NDARRAY_PORT=$(PORT)")

NDFileHDF5Configure("FileHDF1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDFileHDF5.template", "P=$(PREFIX),R=HDF1:,PORT=FileHDF1,NDARRAY_PORT=$(PORT)")

NDFileMagickConfigure("FileMagick1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDFileMagick.template", "P=$(PREFIX),R=Magick1:,PORT=FileMagick1,NDARRAY_PORT=$(PORT)")

# ===== ROI plugins (4 instances) =====

NDROIConfigure("ROI1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDROI.template", "P=$(PREFIX),R=ROI1:,PORT=ROI1,NDARRAY_PORT=$(PORT)")

NDROIConfigure("ROI2", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDROI.template", "P=$(PREFIX),R=ROI2:,PORT=ROI2,NDARRAY_PORT=$(PORT)")

NDROIConfigure("ROI3", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDROI.template", "P=$(PREFIX),R=ROI3:,PORT=ROI3,NDARRAY_PORT=$(PORT)")

NDROIConfigure("ROI4", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDROI.template", "P=$(PREFIX),R=ROI4:,PORT=ROI4,NDARRAY_PORT=$(PORT)")

# ===== ROI statistics (with 32 ROIs via substitute+include) =====

NDROIStatConfigure("ROISTAT1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDROIStat.template", "P=$(PREFIX),R=ROIStat1:,PORT=ROISTAT1,NCHANS=$(NCHANS),NDARRAY_PORT=$(PORT)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:1:,PORT=ROISTAT1,ADDR=0,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:2:,PORT=ROISTAT1,ADDR=1,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:3:,PORT=ROISTAT1,ADDR=2,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:4:,PORT=ROISTAT1,ADDR=3,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:5:,PORT=ROISTAT1,ADDR=4,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:6:,PORT=ROISTAT1,ADDR=5,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:7:,PORT=ROISTAT1,ADDR=6,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:8:,PORT=ROISTAT1,ADDR=7,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:9:,PORT=ROISTAT1,ADDR=8,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:10:,PORT=ROISTAT1,ADDR=9,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:11:,PORT=ROISTAT1,ADDR=10,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:12:,PORT=ROISTAT1,ADDR=11,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:13:,PORT=ROISTAT1,ADDR=12,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:14:,PORT=ROISTAT1,ADDR=13,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:15:,PORT=ROISTAT1,ADDR=14,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:16:,PORT=ROISTAT1,ADDR=15,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:17:,PORT=ROISTAT1,ADDR=16,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:18:,PORT=ROISTAT1,ADDR=17,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:19:,PORT=ROISTAT1,ADDR=18,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:20:,PORT=ROISTAT1,ADDR=19,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:21:,PORT=ROISTAT1,ADDR=20,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:22:,PORT=ROISTAT1,ADDR=21,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:23:,PORT=ROISTAT1,ADDR=22,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:24:,PORT=ROISTAT1,ADDR=23,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:25:,PORT=ROISTAT1,ADDR=24,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:26:,PORT=ROISTAT1,ADDR=25,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:27:,PORT=ROISTAT1,ADDR=26,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:28:,PORT=ROISTAT1,ADDR=27,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:29:,PORT=ROISTAT1,ADDR=28,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:30:,PORT=ROISTAT1,ADDR=29,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:31:,PORT=ROISTAT1,ADDR=30,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDROIStatN.template", "P=$(PREFIX),R=ROIStat1:32:,PORT=ROISTAT1,ADDR=31,TIMEOUT=1,NCHANS=$(NCHANS)")
NDTimeSeriesConfigure("ROISTAT1_TS", $(QSIZE), 0, "ROISTAT1", 0)
dbLoadRecords("NDTimeSeries.template", "P=$(PREFIX),R=ROIStat1:TS:,PORT=ROISTAT1_TS,ADDR=0,TIMEOUT=1,NDARRAY_PORT=ROISTAT1,NDARRAY_ADDR=0,NCHANS=$(NCHANS),ENABLED=1")

# ===== Processing plugin (with helper TIFF for dark/flat field loading) =====

NDProcessConfigure("PROC1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDProcess.template", "P=$(PREFIX),R=Proc1:,PORT=PROC1,NDARRAY_PORT=$(PORT)")

NDFileTIFFConfigure("PROC1TIFF", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDFileTIFF.template", "P=$(PREFIX),R=Proc1:TIFF:,PORT=PROC1TIFF,NDARRAY_PORT=$(PORT)")

# ===== Scatter/Gather =====

NDScatterConfigure("SCATTER1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDScatter.template", "P=$(PREFIX),R=Scatter1:,PORT=SCATTER1,NDARRAY_PORT=$(PORT)")

NDGatherConfigure("GATHER1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDGather.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,NDARRAY_PORT=$(PORT)")
dbLoadRecords("NDGatherN.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,ADDR=0,TIMEOUT=1,NDARRAY_PORT=$(PORT),N=1")
dbLoadRecords("NDGatherN.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,ADDR=1,TIMEOUT=1,NDARRAY_PORT=$(PORT),N=2")
dbLoadRecords("NDGatherN.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,ADDR=2,TIMEOUT=1,NDARRAY_PORT=$(PORT),N=3")
dbLoadRecords("NDGatherN.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,ADDR=3,TIMEOUT=1,NDARRAY_PORT=$(PORT),N=4")
dbLoadRecords("NDGatherN.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,ADDR=4,TIMEOUT=1,NDARRAY_PORT=$(PORT),N=5")
dbLoadRecords("NDGatherN.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,ADDR=5,TIMEOUT=1,NDARRAY_PORT=$(PORT),N=6")
dbLoadRecords("NDGatherN.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,ADDR=6,TIMEOUT=1,NDARRAY_PORT=$(PORT),N=7")
dbLoadRecords("NDGatherN.template", "P=$(PREFIX),R=Gather1:,PORT=GATHER1,ADDR=7,TIMEOUT=1,NDARRAY_PORT=$(PORT),N=8")

# ===== Statistics plugins (5 instances) =====

NDStatsConfigure("STATS1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats1:,PORT=STATS1,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")
NDTimeSeriesConfigure("STATS1_TS", $(QSIZE), 0, "STATS1", 0)
dbLoadRecords("NDTimeSeries.template", "P=$(PREFIX),R=Stats1:TS:,PORT=STATS1_TS,ADDR=0,TIMEOUT=1,NDARRAY_PORT=STATS1,NDARRAY_ADDR=0,NCHANS=$(NCHANS),ENABLED=1")

NDStatsConfigure("STATS2", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats2:,PORT=STATS2,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")
NDTimeSeriesConfigure("STATS2_TS", $(QSIZE), 0, "STATS2", 0)
dbLoadRecords("NDTimeSeries.template", "P=$(PREFIX),R=Stats2:TS:,PORT=STATS2_TS,ADDR=0,TIMEOUT=1,NDARRAY_PORT=STATS2,NDARRAY_ADDR=0,NCHANS=$(NCHANS),ENABLED=1")

NDStatsConfigure("STATS3", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats3:,PORT=STATS3,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")
NDTimeSeriesConfigure("STATS3_TS", $(QSIZE), 0, "STATS3", 0)
dbLoadRecords("NDTimeSeries.template", "P=$(PREFIX),R=Stats3:TS:,PORT=STATS3_TS,ADDR=0,TIMEOUT=1,NDARRAY_PORT=STATS3,NDARRAY_ADDR=0,NCHANS=$(NCHANS),ENABLED=1")

NDStatsConfigure("STATS4", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats4:,PORT=STATS4,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")
NDTimeSeriesConfigure("STATS4_TS", $(QSIZE), 0, "STATS4", 0)
dbLoadRecords("NDTimeSeries.template", "P=$(PREFIX),R=Stats4:TS:,PORT=STATS4_TS,ADDR=0,TIMEOUT=1,NDARRAY_PORT=STATS4,NDARRAY_ADDR=0,NCHANS=$(NCHANS),ENABLED=1")

NDStatsConfigure("STATS5", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDStats.template", "P=$(PREFIX),R=Stats5:,PORT=STATS5,NCHANS=$(NCHANS),XSIZE=$(XSIZE),YSIZE=$(YSIZE),HIST_SIZE=256,NDARRAY_PORT=$(PORT)")
NDTimeSeriesConfigure("STATS5_TS", $(QSIZE), 0, "STATS5", 0)
dbLoadRecords("NDTimeSeries.template", "P=$(PREFIX),R=Stats5:TS:,PORT=STATS5_TS,ADDR=0,TIMEOUT=1,NDARRAY_PORT=STATS5,NDARRAY_ADDR=0,NCHANS=$(NCHANS),ENABLED=1")

# ===== Transform plugin =====

NDTransformConfigure("TRANS1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDTransform.template", "P=$(PREFIX),R=Trans1:,PORT=TRANS1,NDARRAY_PORT=$(PORT)")

# ===== Overlay plugin (with 8 overlays) =====

NDOverlayConfigure("OVER1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDOverlay.template", "P=$(PREFIX),R=Over1:,PORT=OVER1,NDARRAY_PORT=$(PORT)")
dbLoadRecords("NDOverlayN.template", "P=$(PREFIX),R=Over1:1:,NAME=ROI1,SHAPE=1,O=Over1:,XPOS=$(PREFIX)ROI1:MinX_RBV,YPOS=$(PREFIX)ROI1:MinY_RBV,XSIZE=$(PREFIX)ROI1:SizeX_RBV,YSIZE=$(PREFIX)ROI1:SizeY_RBV,PORT=OVER1,ADDR=0,TIMEOUT=1")
dbLoadRecords("NDOverlayN.template", "P=$(PREFIX),R=Over1:2:,NAME=ROI2,SHAPE=1,O=Over1:,XPOS=$(PREFIX)ROI2:MinX_RBV,YPOS=$(PREFIX)ROI2:MinY_RBV,XSIZE=$(PREFIX)ROI2:SizeX_RBV,YSIZE=$(PREFIX)ROI2:SizeY_RBV,PORT=OVER1,ADDR=1,TIMEOUT=1")
dbLoadRecords("NDOverlayN.template", "P=$(PREFIX),R=Over1:3:,NAME=ROI3,SHAPE=1,O=Over1:,XPOS=$(PREFIX)ROI3:MinX_RBV,YPOS=$(PREFIX)ROI3:MinY_RBV,XSIZE=$(PREFIX)ROI3:SizeX_RBV,YSIZE=$(PREFIX)ROI3:SizeY_RBV,PORT=OVER1,ADDR=2,TIMEOUT=1")
dbLoadRecords("NDOverlayN.template", "P=$(PREFIX),R=Over1:4:,NAME=ROI4,SHAPE=1,O=Over1:,XPOS=$(PREFIX)ROI4:MinX_RBV,YPOS=$(PREFIX)ROI4:MinY_RBV,XSIZE=$(PREFIX)ROI4:SizeX_RBV,YSIZE=$(PREFIX)ROI4:SizeY_RBV,PORT=OVER1,ADDR=3,TIMEOUT=1")
dbLoadRecords("NDOverlayN.template", "P=$(PREFIX),R=Over1:5:,NAME=Cursor1,SHAPE=1,O=Over1:,PORT=OVER1,ADDR=4,TIMEOUT=1")
dbLoadRecords("NDOverlayN.template", "P=$(PREFIX),R=Over1:6:,NAME=Cursor2,SHAPE=1,O=Over1:,PORT=OVER1,ADDR=5,TIMEOUT=1")
dbLoadRecords("NDOverlayN.template", "P=$(PREFIX),R=Over1:7:,NAME=Box1,SHAPE=1,O=Over1:,PORT=OVER1,ADDR=6,TIMEOUT=1")
dbLoadRecords("NDOverlayN.template", "P=$(PREFIX),R=Over1:8:,NAME=Box2,SHAPE=1,O=Over1:,PORT=OVER1,ADDR=7,TIMEOUT=1")

# ===== Color conversion plugins (2 instances) =====

NDColorConvertConfigure("CC1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDColorConvert.template", "P=$(PREFIX),R=CC1:,PORT=CC1,NDARRAY_PORT=$(PORT)")

NDColorConvertConfigure("CC2", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDColorConvert.template", "P=$(PREFIX),R=CC2:,PORT=CC2,NDARRAY_PORT=$(PORT)")

# ===== Circular buffer =====

NDCircularBuffConfigure("CB1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDCircularBuff.template", "P=$(PREFIX),R=CB1:,PORT=CB1,NDARRAY_PORT=$(PORT)")

# ===== Attributes (with 8 attribute channels + TimeSeries) =====

NDAttrConfigure("ATTR1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDAttribute.template", "P=$(PREFIX),R=Attr1:,PORT=ATTR1,NCHANS=$(NCHANS),NDARRAY_PORT=$(PORT)")
dbLoadRecords("NDAttributeN.template", "P=$(PREFIX),R=Attr1:1:,PORT=ATTR1,ADDR=0,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDAttributeN.template", "P=$(PREFIX),R=Attr1:2:,PORT=ATTR1,ADDR=1,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDAttributeN.template", "P=$(PREFIX),R=Attr1:3:,PORT=ATTR1,ADDR=2,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDAttributeN.template", "P=$(PREFIX),R=Attr1:4:,PORT=ATTR1,ADDR=3,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDAttributeN.template", "P=$(PREFIX),R=Attr1:5:,PORT=ATTR1,ADDR=4,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDAttributeN.template", "P=$(PREFIX),R=Attr1:6:,PORT=ATTR1,ADDR=5,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDAttributeN.template", "P=$(PREFIX),R=Attr1:7:,PORT=ATTR1,ADDR=6,TIMEOUT=1,NCHANS=$(NCHANS)")
dbLoadRecords("NDAttributeN.template", "P=$(PREFIX),R=Attr1:8:,PORT=ATTR1,ADDR=7,TIMEOUT=1,NCHANS=$(NCHANS)")
NDTimeSeriesConfigure("ATTR1_TS", $(QSIZE), 0, "ATTR1", 0)
dbLoadRecords("NDTimeSeries.template", "P=$(PREFIX),R=Attr1:TS:,PORT=ATTR1_TS,ADDR=0,TIMEOUT=1,NDARRAY_PORT=ATTR1,NDARRAY_ADDR=0,NCHANS=$(NCHANS),ENABLED=1")

# ===== FFT =====

NDFFTConfigure("FFT1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDFFT.template", "P=$(PREFIX),R=FFT1:,PORT=FFT1,NAME=FFT1,NCHANS=$(NCHANS),NDARRAY_PORT=$(PORT)")

# ===== Codec plugins (2 instances) =====

NDCodecConfigure("CODEC1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDCodec.template", "P=$(PREFIX),R=Codec1:,PORT=CODEC1,NDARRAY_PORT=$(PORT)")

NDCodecConfigure("CODEC2", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDCodec.template", "P=$(PREFIX),R=Codec2:,PORT=CODEC2,NDARRAY_PORT=$(PORT)")

# ===== Bad pixel =====

NDBadPixelConfigure("BADPIX1", $(QSIZE), 0, "$(PORT)", 0)
dbLoadRecords("NDBadPixel.template", "P=$(PREFIX),R=BadPix1:,PORT=BADPIX1,NDARRAY_PORT=$(PORT)")

# ===== PVA plugin =====

NDPvaConfigure("PVA1", $(QSIZE), 0, "$(PORT)", 0, "$(PREFIX)Pva1:Image")
dbLoadRecords("NDPva.template", "P=$(PREFIX),R=Pva1:,PORT=PVA1,NDARRAY_PORT=$(PORT)")
