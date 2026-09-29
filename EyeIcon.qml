import QtQuick
import QtQuick.Shapes
import qs.Commons

// The Eye of Sauron atop Barad-dûr, drawn natively so it stays crisp in a
// 16px bar slot. The tower takes the theme colour; the eye is always fire
// with a black slit. `openness` lowers the lid (1 open, 0 shut), `wary`
// dilates the pupil, and `seeing` sets the flames moving.
Item {
  id: root

  property real iconSize: Style.font.icon
  property color color: Color.foreground
  property real openness: 1
  property bool wary: false
  property bool seeing: false

  // Sampled from the reference art: burnt orange with hot highlights.
  readonly property color fireCore: "#f6aa55"
  readonly property color fireMid: "#dc6a2a"
  readonly property color fireEdge: "#a33d1a"
  readonly property color pupilColor: "#120806"

  implicitWidth: iconSize
  implicitHeight: iconSize

  property real lid: openness
  Behavior on lid { NumberAnimation { duration: Style.duration(360); easing.type: Easing.InOutCubic } }

  property real dilation: wary ? 1 : 0
  Behavior on dilation { NumberAnimation { duration: Style.duration(420); easing.type: Easing.OutCubic } }

  // Two unsynchronised drifts keep the flames from looking like a metronome.
  property real flame: 0
  property real drift: 0
  SequentialAnimation on flame {
    running: root.seeing && !Style.reduceMotion
    loops: Animation.Infinite
    onRunningChanged: if (!running) root.flame = 0
    NumberAnimation { to: 1; duration: 540; easing.type: Easing.InOutSine }
    NumberAnimation { to: 0.2; duration: 760; easing.type: Easing.InOutSine }
  }
  SequentialAnimation on drift {
    running: root.seeing && !Style.reduceMotion
    loops: Animation.Infinite
    onRunningChanged: if (!running) root.drift = 0
    NumberAnimation { to: 1; duration: 1300; easing.type: Easing.InOutSine }
    NumberAnimation { to: -1; duration: 1700; easing.type: Easing.InOutSine }
  }

  readonly property real u: iconSize

  // ---------------------------------------------------------------- tower
  readonly property real outerL: u * 0.06
  readonly property real outerR: u * 0.94
  readonly property real tipY: 0
  readonly property real shoulderY: u * 0.8
  readonly property real shaftY: u * 0.88
  readonly property real shaftL: u * 0.26
  readonly property real shaftR: u * 0.74
  // The cradle is one cubic; its lowest point sits at tipY + 0.75 * cradleDepth.
  readonly property real cradleDepth: u * 0.907

  // ---------------------------------------------------------------- eye
  // Set low enough in the cradle that the horns rise well above it.
  readonly property real cx: u / 2
  readonly property real eyeY: u * 0.34
  readonly property real eyeHalf: u * 0.37
  // A quadratic lid peaks halfway to its control point.
  readonly property real reach: u * 0.36
  readonly property real upper: eyeY - reach + 2 * reach * (1 - lid)
  readonly property real lower: eyeY + reach
  readonly property real pupilTop: (eyeY + upper) / 2 + u * 0.02
  readonly property real pupilBottom: (eyeY + lower) / 2 - u * 0.02
  readonly property bool pupilVisible: pupilBottom - pupilTop > u * 0.06
  readonly property real slit: u * (0.11 + 0.08 * dilation + 0.02 * flame)

  function mix(a, b, t) {
    return Qt.rgba(a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t, a.a + (b.a - a.a) * t)
  }

  Shape {
    anchors.fill: parent
    preferredRendererType: Shape.CurveRenderer

    // Barad-dûr: two horns cupping the eye, stepping in to the shaft.
    ShapePath {
      strokeColor: "transparent"
      fillColor: root.color
      startX: root.outerL; startY: root.tipY
      PathLine { x: root.outerL; y: root.shoulderY }
      PathLine { x: root.shaftL; y: root.shaftY }
      PathLine { x: root.shaftL; y: root.u }
      PathLine { x: root.shaftR; y: root.u }
      PathLine { x: root.shaftR; y: root.shaftY }
      PathLine { x: root.outerR; y: root.shoulderY }
      PathLine { x: root.outerR; y: root.tipY }
      PathCubic {
        x: root.outerL; y: root.tipY
        control1X: root.outerR; control1Y: root.tipY + root.cradleDepth
        control2X: root.outerL; control2Y: root.tipY + root.cradleDepth
      }
    }
  }

  // A shut lid leaves a zero-area eye that would still rasterise as a hairline.
  Shape {
    anchors.fill: parent
    visible: root.lid > 0.03
    preferredRendererType: Shape.CurveRenderer

    // The eye: fire between the lids.
    ShapePath {
      strokeColor: "transparent"
      fillGradient: RadialGradient {
        centerX: root.cx
        centerY: root.eyeY
        centerRadius: root.eyeHalf
        focalX: root.cx + root.drift * root.u * 0.08
        focalY: root.eyeY - root.flame * root.u * 0.03
        GradientStop { position: 0.0; color: root.mix(root.fireCore, "#ffe2a0", root.flame * 0.6) }
        GradientStop { position: 0.55; color: root.mix(root.fireMid, root.fireCore, root.flame * 0.35) }
        GradientStop { position: 1.0; color: root.fireEdge }
      }
      startX: root.cx - root.eyeHalf; startY: root.eyeY
      PathQuad { x: root.cx + root.eyeHalf; y: root.eyeY; controlX: root.cx; controlY: root.upper }
      PathQuad { x: root.cx - root.eyeHalf; y: root.eyeY; controlX: root.cx; controlY: root.lower }
    }

    // The slit, hanging from under the upper lid.
    ShapePath {
      strokeColor: "transparent"
      fillColor: root.pupilVisible ? root.pupilColor : "transparent"
      startX: root.cx; startY: root.pupilTop
      PathQuad { x: root.cx; y: root.pupilBottom; controlX: root.cx + root.slit; controlY: (root.pupilTop + root.pupilBottom) / 2 }
      PathQuad { x: root.cx; y: root.pupilTop; controlX: root.cx - root.slit; controlY: (root.pupilTop + root.pupilBottom) / 2 }
    }
  }
}
