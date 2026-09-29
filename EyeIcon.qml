import QtQuick
import QtQuick.Shapes
import qs.Commons

// The Eye: an almond lid around a vertical slit pupil, drawn natively so it
// stays crisp in a 16px bar slot. `openness` lowers the upper lid (1 open,
// 0 shut); `fire` floods the iris with flame in the theme's urgent colour.
Item {
  id: root

  property real iconSize: Style.font.icon
  property color color: Color.foreground
  property color fireColor: Color.urgent
  property color pupilColor: Color.background
  property real openness: 1
  property real fire: 0
  property bool pulsing: false

  implicitWidth: iconSize
  implicitHeight: iconSize

  property real lid: openness
  Behavior on lid { NumberAnimation { duration: Style.duration(360); easing.type: Easing.InOutCubic } }

  property real heat: fire
  Behavior on heat { NumberAnimation { duration: Style.duration(260); easing.type: Easing.OutCubic } }

  property real flicker: 1
  SequentialAnimation on flicker {
    running: root.pulsing && root.fire > 0 && !Style.reduceMotion
    loops: Animation.Infinite
    onRunningChanged: if (!running) root.flicker = 1
    NumberAnimation { to: 0.5; duration: 640; easing.type: Easing.InOutSine }
    NumberAnimation { to: 1; duration: 860; easing.type: Easing.InOutSine }
  }

  readonly property real stroke: Math.max(1.2, iconSize * 0.085)
  readonly property real cornerL: stroke / 2
  readonly property real cornerR: iconSize - stroke / 2
  readonly property real mid: iconSize / 2
  // A quadratic curve peaks halfway to its control point, so a control
  // `reach` away puts each lid's apex at reach / 2 from the centre line.
  readonly property real reach: iconSize * 0.56
  readonly property real upper: mid - reach + 2 * reach * (1 - lid)
  readonly property real lower: mid + reach
  // The slit hangs from under the upper lid, so a lowered lid hides its top.
  readonly property real pupilTop: Math.max(mid - iconSize * 0.22, (mid + upper) / 2 + stroke * 0.7)
  readonly property real pupilBottom: mid + iconSize * 0.22
  readonly property real pupilMid: (pupilTop + pupilBottom) / 2
  readonly property bool pupilVisible: pupilBottom - pupilTop > stroke
  readonly property real slitWidth: iconSize * (0.11 + 0.07 * heat)

  function mix(a, b, t) {
    return Qt.rgba(a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t, a.a + (b.a - a.a) * t)
  }

  // Lid curve height at parameter t (the lower lid; x is linear in t).
  function lidY(t) { return mid + 2 * t * (1 - t) * reach }
  function lidX(t) { return cornerL + (cornerR - cornerL) * t }
  readonly property real lashLength: iconSize * 0.15
  readonly property real lashAlpha: Math.max(0, 1 - lid * 2.5)

  Shape {
    anchors.fill: parent
    preferredRendererType: Shape.CurveRenderer
    // A shut eye is only the lower curve; lift it back to the optical centre.
    // Squared so a half-lowered lid stays near centre and only a shut one lifts.
    transform: Translate { y: -root.reach * 0.36 * (1 - root.lid) * (1 - root.lid) }

    ShapePath {
      strokeColor: "transparent"
      fillGradient: RadialGradient {
        centerX: root.mid
        centerY: root.mid
        centerRadius: root.iconSize * 0.46
        focalX: root.mid
        focalY: root.mid
        GradientStop { position: 0.0; color: Util.alpha(Qt.lighter(root.fireColor, 1.7), root.heat * root.flicker) }
        GradientStop { position: 0.5; color: Util.alpha(root.fireColor, 0.9 * root.heat * root.flicker) }
        GradientStop { position: 1.0; color: Util.alpha(root.fireColor, 0.15 * root.heat) }
      }
      startX: root.cornerL; startY: root.mid
      PathQuad { x: root.cornerR; y: root.mid; controlX: root.mid; controlY: root.upper }
      PathQuad { x: root.cornerL; y: root.mid; controlX: root.mid; controlY: root.lower }
    }

    ShapePath {
      strokeColor: root.color
      strokeWidth: root.stroke
      fillColor: "transparent"
      capStyle: ShapePath.RoundCap
      joinStyle: ShapePath.RoundJoin
      startX: root.cornerL; startY: root.mid
      PathQuad { x: root.cornerR; y: root.mid; controlX: root.mid; controlY: root.upper }
      PathQuad { x: root.cornerL; y: root.mid; controlX: root.mid; controlY: root.lower }
    }

    ShapePath {
      strokeColor: "transparent"
      fillColor: root.pupilVisible ? root.mix(root.color, root.pupilColor, root.heat) : "transparent"
      startX: root.mid; startY: root.pupilTop
      PathQuad { x: root.mid; y: root.pupilBottom; controlX: root.mid + root.slitWidth; controlY: root.pupilMid }
      PathQuad { x: root.mid; y: root.pupilTop; controlX: root.mid - root.slitWidth; controlY: root.pupilMid }
    }

    ShapePath {
      strokeColor: Util.alpha(root.color, root.color.a * root.lashAlpha)
      strokeWidth: root.stroke * 0.85
      fillColor: "transparent"
      capStyle: ShapePath.RoundCap
      startX: root.lidX(0.25); startY: root.lidY(0.25)
      PathLine { x: root.lidX(0.25) - root.lashLength * 0.45; y: root.lidY(0.25) + root.lashLength * 0.8 }
      PathMove { x: root.lidX(0.5); y: root.lidY(0.5) }
      PathLine { x: root.lidX(0.5); y: root.lidY(0.5) + root.lashLength }
      PathMove { x: root.lidX(0.75); y: root.lidY(0.75) }
      PathLine { x: root.lidX(0.75) + root.lashLength * 0.45; y: root.lidY(0.75) + root.lashLength * 0.8 }
    }
  }
}
