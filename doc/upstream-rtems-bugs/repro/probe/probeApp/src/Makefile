TOP=../..
include $(TOP)/configure/CONFIG

PROD_IOC = probe
probe_SRCS += probeMain.c
USR_LDFLAGS += -Wl,--wrap=poll
probe_LIBS += pvxs
probe_LIBS += $(EPICS_BASE_IOC_LIBS)

include $(TOP)/configure/RULES
