#!/usr/bin/env python
"""
PyDM AreaDetector Image Viewer for xrt-beamline.

Displays the beam profile from the NDStdArrays plugin and
key simulation readback PVs.

Usage:
    python pydm_viewer.py    # a Python with pydm
"""

import os
os.environ.setdefault("EPICS_CA_AUTO_ADDR_LIST", "YES")

from pydm import Display
from pydm.widgets import (
    PyDMImageView, PyDMLabel, PyDMLineEdit,
    PyDMPushButton, PyDMEnumComboBox,
)
from qtpy.QtWidgets import (
    QVBoxLayout, QHBoxLayout, QGridLayout,
    QGroupBox, QLabel, QApplication, QSplitter,
    QSlider, QCheckBox,
)
from qtpy.QtCore import Qt
import sys

PREFIX = "bl:xrt:"
CAM = f"{PREFIX}cam1:"
IMG = f"{PREFIX}image1:"


class BeamlineViewer(Display):
    def __init__(self, parent=None, args=None, macros=None):
        super().__init__(parent=parent, args=args, macros=macros)
        self.setWindowTitle("XRT Beamline - AreaDetector Viewer")
        self.setup_ui()

    def setup_ui(self):
        main_layout = QHBoxLayout()
        self.setLayout(main_layout)

        splitter = QSplitter(Qt.Horizontal)
        main_layout.addWidget(splitter)

        # ===== Left: Image View =====
        image_widget = QGroupBox("Beam Profile")
        image_layout = QVBoxLayout()
        image_widget.setLayout(image_layout)

        self.image_view = PyDMImageView(
            image_channel=f"ca://{IMG}ArrayData",
            width_channel=f"ca://{CAM}ArraySizeX_RBV",
        )
        self.image_view.setMinimumSize(512, 512)
        self.image_view.colorMap = self.image_view.Inferno
        self.image_view.readingOrder = self.image_view.Clike
        self.image_view.setColorMapLimits(0, 65535)
        self.image_view.getView().invertY(False)
        image_layout.addWidget(self.image_view)

        # Contrast controls
        contrast_layout = QHBoxLayout()
        self.auto_cb = QCheckBox("Auto")
        self.auto_cb.setChecked(True)
        self.auto_cb.toggled.connect(self._on_auto_toggle)
        contrast_layout.addWidget(self.auto_cb)

        contrast_layout.addWidget(QLabel("Max:"))
        self.max_slider = QSlider(Qt.Horizontal)
        self.max_slider.setRange(1, 65535)
        self.max_slider.setValue(65535)
        self.max_slider.valueChanged.connect(self._on_max_changed)
        contrast_layout.addWidget(self.max_slider)

        self.max_label = QLabel("65535")
        self.max_label.setFixedWidth(50)
        contrast_layout.addWidget(self.max_label)
        image_layout.addLayout(contrast_layout)

        # Acquire controls
        acq_layout = QHBoxLayout()
        acq_start = PyDMPushButton(
            label="Start", init_channel=f"ca://{CAM}Acquire",
            pressValue=1)
        acq_stop = PyDMPushButton(
            label="Stop", init_channel=f"ca://{CAM}Acquire",
            pressValue=0)
        acq_layout.addWidget(acq_start)
        acq_layout.addWidget(acq_stop)
        acq_layout.addWidget(QLabel("Callbacks:"))
        cb_on = PyDMPushButton(
            label="On", init_channel=f"ca://{CAM}ArrayCallbacks",
            pressValue=1)
        acq_layout.addWidget(cb_on)
        image_layout.addLayout(acq_layout)

        splitter.addWidget(image_widget)

        # ===== Right: Readbacks + Motor Controls =====
        right_widget = QGroupBox("Beamline Status")
        right_layout = QVBoxLayout()
        right_widget.setLayout(right_layout)

        # Simulation readbacks
        rb_group = QGroupBox("Simulation")
        rb_grid = QGridLayout()
        rb_group.setLayout(rb_grid)

        readbacks = [
            ("Energy (src)", f"{CAM}SrcEnergy_RBV", "eV"),
            ("Energy (DCM)", f"{CAM}DcmEnergy_RBV", "eV"),
            ("Efficiency", f"{CAM}Efficiency_RBV", "%"),
            ("Flux", f"{CAM}Flux_RBV", ""),
            ("Centroid X", f"{CAM}CentroidX_RBV", "mm"),
            ("Centroid Z", f"{CAM}CentroidZ_RBV", "mm"),
            ("FWHM X", f"{CAM}FWHMX_RBV", "mm"),
            ("FWHM Z", f"{CAM}FWHMZ_RBV", "mm"),
            ("N Rays", f"{CAM}NRays_RBV", ""),
        ]
        for i, (name, pv, unit) in enumerate(readbacks):
            rb_grid.addWidget(QLabel(name), i, 0)
            lbl = PyDMLabel(init_channel=f"ca://{pv}")
            lbl.precisionFromChannel = False
            lbl.precision = 2
            rb_grid.addWidget(lbl, i, 1)
            rb_grid.addWidget(QLabel(unit), i, 2)

        right_layout.addWidget(rb_group)

        # Motor controls
        motors = [
            ("DCM Theta", "bl:dcm:theta", "deg"),
            ("DCM Y", "bl:dcm:y", "mm"),
            ("HFM Pitch", "bl:hfm:pitch", "mrad"),
            ("HFM R", "bl:hfm:rmaj", "mm"),
            ("VFM Pitch", "bl:vfm:pitch", "mrad"),
            ("VFM R", "bl:vfm:rmaj", "mm"),
            ("Und Gap", "bl:und:gap", "mm"),
        ]

        mtr_group = QGroupBox("Motors")
        mtr_grid = QGridLayout()
        mtr_group.setLayout(mtr_grid)

        mtr_grid.addWidget(QLabel("Motor"), 0, 0)
        mtr_grid.addWidget(QLabel("Setpoint"), 0, 1)
        mtr_grid.addWidget(QLabel("Readback"), 0, 2)
        mtr_grid.addWidget(QLabel("Unit"), 0, 3)

        for i, (name, pv, unit) in enumerate(motors, start=1):
            mtr_grid.addWidget(QLabel(name), i, 0)
            sp = PyDMLineEdit(init_channel=f"ca://{pv}")
            sp.setMaximumWidth(100)
            mtr_grid.addWidget(sp, i, 1)
            rbv = PyDMLabel(init_channel=f"ca://{pv}.RBV")
            rbv.precisionFromChannel = False
            rbv.precision = 4
            mtr_grid.addWidget(rbv, i, 2)
            mtr_grid.addWidget(QLabel(unit), i, 3)

        right_layout.addWidget(mtr_group)

        # Array info
        info_group = QGroupBox("Detector")
        info_grid = QGridLayout()
        info_group.setLayout(info_grid)

        info_pvs = [
            ("Size X", f"{CAM}SizeX_RBV"),
            ("Size Y", f"{CAM}SizeY_RBV"),
            ("Counter", f"{CAM}ArrayCounter_RBV"),
            ("Status", f"{CAM}DetectorState_RBV"),
        ]
        for i, (name, pv) in enumerate(info_pvs):
            info_grid.addWidget(QLabel(name), i, 0)
            info_grid.addWidget(PyDMLabel(init_channel=f"ca://{pv}"), i, 1)

        right_layout.addWidget(info_group)
        right_layout.addStretch()

        splitter.addWidget(right_widget)
        splitter.setSizes([600, 300])

    def _on_auto_toggle(self, checked):
        if checked:
            self.image_view.autoLevels = True
            self.max_slider.setEnabled(False)
        else:
            self.image_view.autoLevels = False
            self.max_slider.setEnabled(True)
            self._on_max_changed(self.max_slider.value())

    def _on_max_changed(self, value):
        self.max_label.setText(str(value))
        if not self.auto_cb.isChecked():
            self.image_view.setColorMapLimits(0, value)

    def ui_filename(self):
        return None

    def ui_filepath(self):
        return None


def main():
    app = QApplication(sys.argv)
    viewer = BeamlineViewer()
    viewer.resize(950, 600)
    viewer.show()
    sys.exit(app.exec_())


if __name__ == '__main__':
    main()
