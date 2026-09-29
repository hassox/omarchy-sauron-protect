import QtQuick
import QtQuick.Effects
import qs.Commons

// A camera still that refreshes without flashing: each new snapshot decodes
// into the buffer underneath and fades in over the previous frame only once
// it is ready, so the grid never drops to an empty frame between refreshes.
Item {
  id: root

  property string path: ""
  property int seq: 0
  property real radius: Style.cornerRadius
  property color placeholder: Qt.darker(Color.popups.background, 1.25)

  readonly property string url: path !== "" && seq > 0 ? Util.fileUrl(path) + "?v=" + seq : ""
  property Image front: null
  readonly property bool ready: front !== null

  onUrlChanged: if (url !== "") (front === a ? b : a).source = url

  function promote(image) {
    if (image === front || image.status !== Image.Ready || String(image.source) !== url) return
    var previous = front
    image.z = 2
    image.opacity = 0
    if (previous) previous.z = 1
    front = image
    fadeIn.target = image
    fadeIn.restart()
  }

  NumberAnimation {
    id: fadeIn
    property: "opacity"
    to: 1
    duration: Style.duration(220)
    easing.type: Easing.OutCubic
  }

  Rectangle {
    id: mask
    anchors.fill: parent
    radius: root.radius
    color: "white"
    visible: false
    layer.enabled: root.radius > 0
  }

  Item {
    anchors.fill: parent
    layer.enabled: root.radius > 0
    layer.effect: MultiEffect {
      maskEnabled: true
      maskSource: mask
      maskThresholdMin: 0.5
      maskSpreadAtMin: 1.0
    }

    Rectangle {
      anchors.fill: parent
      color: root.placeholder
    }

    Image {
      id: a
      anchors.fill: parent
      asynchronous: true
      cache: false
      smooth: true
      fillMode: Image.PreserveAspectCrop
      opacity: 0
      onStatusChanged: root.promote(a)
    }

    Image {
      id: b
      anchors.fill: parent
      asynchronous: true
      cache: false
      smooth: true
      fillMode: Image.PreserveAspectCrop
      opacity: 0
      onStatusChanged: root.promote(b)
    }
  }
}
