import QtQuick
import QtQuick.Controls
import Quickshell
import qs.Commons
import qs.Ui
import "Kinds.js" as Kinds

// The bar half of the plugin: the eye in the bar, and the panel it opens —
// detection filters, silence, a live snapshot grid, and recent sightings.
// All state lives in Service.qml; each monitor's copy of this widget reads it.
Panel {
  id: root
  moduleName: "sauron"
  manageIpc: false

  readonly property string pluginId: "sauron"
  readonly property var sauron: bar && bar.shell ? bar.shell.serviceFor(pluginId) : null
  readonly property string watchKey: "panel-" + Math.random().toString(36).slice(2)

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  // ------------------------------------------------------------ service mirror
  readonly property string status: sauron ? sauron.status : "starting"
  readonly property bool online: status === "online"
  readonly property var cameras: sauron ? sauron.cameras : []
  readonly property var recent: sauron ? sauron.recent : []
  readonly property var activeByCamera: sauron ? sauron.activeByCamera : ({})
  readonly property var snapshots: sauron ? sauron.snapshots : ({})
  readonly property var notifyKinds: sauron ? sauron.notifyKinds : Kinds.defaultNotify
  readonly property var kinds: Kinds.available(cameras)
  readonly property bool silenced: sauron ? sauron.silenced : false
  readonly property bool dnd: sauron ? sauron.dnd : false
  readonly property bool alerting: sauron ? sauron.alerting : false
  readonly property int unseen: sauron ? sauron.unseen : 0

  // ------------------------------------------------------------ the eye
  readonly property real eyeOpenness: online ? (silenced ? 0.4 : 1) : 0
  // Dilated: something was seen (now, or since the panel was last opened).
  readonly property bool eyeWary: online && !silenced && (alerting || unseen > 0)
  // Flames move only while a detection is live and nothing asks for quiet.
  readonly property bool eyeSeeing: alerting && !silenced && !dnd

  readonly property string silenceUntilText: {
    if (!sauron || !silenced) return ""
    return sauron.silencedUntil < 0 ? "" : Kinds.clock(sauron.silencedUntil)
  }

  readonly property string alertText: sauron && alerting
    ? Kinds.describe(sauron.alertKinds) + " · " + sauron.cameraName(sauron.alertCamera)
    : ""

  // Not connected: one short status line and the one thing that fixes it.
  readonly property string statusTitle: {
    switch (status) {
    case "missing": return "Daemon not installed"
    case "outdated": return "Daemon out of date"
    case "unconfigured": return "Not set up"
    case "auth": return "API key rejected"
    case "offline": return "Can't reach your console"
    default: return ""
    }
  }
  readonly property string actionLabel: {
    switch (status) {
    case "missing": return "Install"
    case "outdated": return "Update"
    case "unconfigured":
    case "auth": return "Set up"
    case "offline": return "Retry"
    default: return ""
    }
  }
  // The daemon's own words only where they say something the title doesn't:
  // a network error, or a config the wizard didn't write.
  readonly property string statusDetail: {
    if (!sauron) return ""
    if (status === "offline") return sauron.message
    if (status === "unconfigured" && sauron.message.indexOf("Not set up") !== 0) return sauron.message
    return ""
  }

  readonly property string via: sauron ? sauron.via : ""

  readonly property string heroMeta: {
    if (!online) return statusTitle
    if (alerting) return alertText
    if (dnd) return "Do Not Disturb"
    return via !== "" ? "via " + via : ""
  }

  readonly property string tooltip: {
    if (!online) return "Sauron" + (statusTitle !== "" ? " · " + statusTitle : "")
    if (alerting) return alertText
    return "Sauron · " + (sauron ? sauron.onlineCount : 0) + "/" + cameras.length + " cameras"
      + (via !== "" ? " · via " + via : "") + (silenced ? " · silenced" : "")
  }

  function runAction() {
    if (!sauron || actionLabel === "") return
    if (status === "offline") { sauron.reload(); return }
    close()
    if (status === "missing" || status === "outdated") sauron.install()
    else sauron.setup()
  }

  // ------------------------------------------------------------ grid geometry
  readonly property int columns: cameras.length > 4 ? 3 : 2
  readonly property int gridGap: Style.space(8)
  // Desired grid width; tiles then divide whatever width the card really has.
  readonly property int panelWidth: columns * Style.space(columns === 3 ? 196 : 232) + (columns - 1) * gridGap
  readonly property real tileWidth: Math.floor((column.width - (columns - 1) * gridGap) / columns)

  // ------------------------------------------------------------ cursor model
  readonly property var silenceOptions: [
    { label: "30 min", minutes: 30 },
    { label: "1 hour", minutes: 60 },
    { label: "8 hours", minutes: 480 },
    { label: "Until woken", minutes: 0 }
  ]
  readonly property var sections: {
    var list = []
    if (actionLabel !== "") list.push("action")
    if (online && kinds.length > 0) list.push("kinds")
    if (online) list.push("silence")
    if (online && cameras.length > 0) list.push("tiles")
    if (recent.length > 0) list.push("events")
    return list
  }
  property bool cursorActive: false
  property string section: "tiles"
  property int kindIndex: 0
  property int silenceIndex: 0
  property int tileIndex: 0
  property int eventIndex: 0

  function sectionSize(name) {
    if (name === "action") return 1
    if (name === "kinds") return kinds.length
    if (name === "silence") return silenceOptions.length
    if (name === "tiles") return cameras.length
    if (name === "events") return recent.length
    return 0
  }

  function indexOf(name) {
    if (name === "kinds") return kindIndex
    if (name === "silence") return silenceIndex
    if (name === "tiles") return tileIndex
    if (name === "events") return eventIndex
    return 0
  }

  function setIndex(name, i) {
    var n = sectionSize(name)
    var v = Math.max(0, Math.min(n - 1, i))
    if (name === "kinds") kindIndex = v
    else if (name === "silence") silenceIndex = v
    else if (name === "tiles") tileIndex = v
    else if (name === "events") eventIndex = v
  }

  function setCursor(name, i) {
    cursorActive = true
    section = name
    setIndex(name, i)
  }

  function ensureCursor() {
    if (sections.length === 0) { cursorActive = false; return }
    if (sections.indexOf(section) < 0) section = sections.indexOf("tiles") >= 0 ? "tiles" : sections[0]
    setIndex(section, indexOf(section))
  }

  function moveSection(delta) {
    var i = sections.indexOf(section) + delta
    if (i < 0 || i >= sections.length) return
    var from = section
    section = sections[i]
    if (section === "tiles" && from !== "tiles") setIndex("tiles", delta > 0 ? 0 : cameras.length - 1)
    else setIndex(section, indexOf(section))
  }

  function moveCursor(dx, dy) {
    ensureCursor()
    if (!cursorActive) { cursorActive = true; return }
    var i = indexOf(section)
    var n = sectionSize(section)
    if (section === "tiles") {
      if (dx !== 0) {
        var nx = i + dx
        if (nx >= 0 && nx < n && Math.floor(nx / columns) === Math.floor(i / columns)) tileIndex = nx
        return
      }
      var ny = i + dy * columns
      if (ny >= 0 && ny < n) { tileIndex = ny; return }
      if (dy > 0 && Math.floor(i / columns) < Math.floor((n - 1) / columns)) { tileIndex = n - 1; return }
      moveSection(dy)
      return
    }
    if (section === "events") {
      if (dy === 0) return
      var ne = i + dy
      if (ne >= 0 && ne < n) eventIndex = ne
      else moveSection(dy)
      return
    }
    if (dx !== 0) setIndex(section, i + dx)
    else moveSection(dy)
  }

  function activateCursor() {
    if (!sauron) return
    if (section === "action") runAction()
    else if (section === "kinds") sauron.toggleKind(kinds[kindIndex])
    else if (section === "silence") chooseSilence(silenceOptions[silenceIndex])
    else if (section === "tiles") openCamera(tileIndex)
    else if (section === "events" && recent[eventIndex]) showCamera(recent[eventIndex].camera)
  }

  function chooseSilence(option) {
    if (!sauron || !option) return
    if (option.minutes === 0 && silenced && sauron.silencedUntil < 0) sauron.resume()
    else sauron.silence(option.minutes)
  }

  function openCamera(i) {
    if (cameras[i]) showCamera(cameras[i].id)
  }

  // The panel holds keyboard focus while open, and Hyprland won't hand focus
  // to a window that maps underneath it. Close first so the player gets focus.
  function showCamera(id) {
    if (!sauron || !id || !online) return
    close()
    sauron.openLive(id)
  }

  function scrollIntoView(item) {
    if (!item || !panelFlick) return
    var p = item.mapToItem(panelFlick.contentItem, 0, 0)
    if (p.y < panelFlick.contentY) panelFlick.contentY = p.y
    else if (p.y + item.height > panelFlick.contentY + panelFlick.height)
      panelFlick.contentY = Math.min(panelFlick.contentHeight - panelFlick.height, p.y + item.height - panelFlick.height)
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onOpenedChanged: {
    if (sauron) sauron.setWatching(watchKey, opened)
    if (!opened) return
    cursorActive = false
    if (panelFlick) panelFlick.contentY = 0
    Qt.callLater(function() { keyCatcher.forceActiveFocus() })
  }
  onSectionsChanged: if (cursorActive) ensureCursor()
  Component.onDestruction: if (sauron) sauron.setWatching(watchKey, false)

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    tooltipText: root.tooltip
    iconComponent: Component {
      Item {
        EyeIcon {
          anchors.centerIn: parent
          iconSize: Style.bar.iconCanvas
          color: root.online ? root.barForeground : Qt.darker(root.barForeground, 1.55)
          openness: root.eyeOpenness
          wary: root.eyeWary
          seeing: root.eyeSeeing
        }
      }
    }
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton) { if (root.sauron) root.sauron.openLatest() }
      else if (buttonCode === Qt.MiddleButton) { if (root.sauron) root.sauron.toggleSilence() }
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(root.panelWidth + panel.padding * 2 + Border.left(panel.borderSpec) + Border.right(panel.borderSpec))
    contentHeight: panel.fittedContentHeight(column.implicitHeight, Style.space(900))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onMoveRequested: function(dx, dy) { root.moveCursor(dx, dy) }
      // Enter with no cursor yet goes straight to the fix when there is one.
      onActivateRequested: {
        if (root.cursorActive) root.activateCursor()
        else root.runAction()
      }
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (!root.sauron) return
        if (t >= "1" && t <= "9") root.openCamera(Number(t) - 1)
        else if (t === "s" || t === "S") root.sauron.toggleSilence()
        else if (t === "m" || t === "M") { if (root.cursorActive && root.section === "tiles" && root.cameras[root.tileIndex]) root.sauron.toggleMuted(root.cameras[root.tileIndex].id) }
        else if (t === "r" || t === "R") root.sauron.reload()
      }

      Flickable {
        id: panelFlick
        anchors.fill: parent
        contentWidth: width
        contentHeight: column.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        flickableDirection: Flickable.VerticalFlick
        interactive: contentHeight > height
        ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

        Column {
          id: column
          width: panelFlick.width
          spacing: Style.space(12)

          PanelHero {
            id: hero
            width: parent.width
            title: "Sauron"
            meta: root.heroMeta
            detail: root.online && root.cameras.length > 0 ? (root.sauron.onlineCount + "/" + root.cameras.length) : ""
            foreground: root.foreground
            fontFamily: root.fontFamily
            iconComponent: Component {
              EyeIcon {
                iconSize: Style.space(34)
                color: root.online ? root.foreground : root.dim
                openness: root.eyeOpenness
                wary: root.eyeWary
                seeing: root.eyeSeeing
              }
            }
            trailingControl: Component {
              ToggleSwitch {
                visible: root.online
                checked: !root.silenced
                foreground: root.foreground
                hasCursor: false
                onToggled: if (root.sauron) root.sauron.toggleSilence()
              }
            }
          }

          Column {
            visible: root.actionLabel !== ""
            width: parent.width
            spacing: Style.spacing.lg

            Text {
              visible: root.statusDetail !== ""
              width: parent.width
              textFormat: Text.PlainText
              text: root.statusDetail
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WrapAtWordBoundaryOrAnywhere
            }

            Button {
              text: root.actionLabel
              selected: true
              bordered: true
              hasCursor: root.cursorActive && root.section === "action"
              foreground: root.foreground
              fontFamily: root.fontFamily
              fontSize: Style.font.bodySmall
              horizontalPadding: Style.space(18)
              onClicked: root.runAction()
              onHovered: function(isHovered) { if (isHovered) root.setCursor("action", 0) }
            }
          }

          // ---------- Alert on ----------
          Column {
            visible: root.sections.indexOf("kinds") >= 0
            width: parent.width
            spacing: Style.spacing.lg

            PanelSectionHeader {
              text: "ALERT ON"
              foreground: root.foreground
              fontFamily: root.fontFamily
            }

            Flow {
              width: parent.width
              spacing: Style.spacing.md

              Repeater {
                model: root.kinds

                Button {
                  required property var modelData
                  required property int index
                  text: Kinds.label(modelData)
                  selected: root.notifyKinds.indexOf(modelData) >= 0
                  hasCursor: root.cursorActive && root.section === "kinds" && root.kindIndex === index
                  bordered: true
                  foreground: root.foreground
                  fontFamily: root.fontFamily
                  fontSize: Style.font.bodySmall
                  onClicked: {
                    root.setCursor("kinds", index)
                    if (root.sauron) root.sauron.toggleKind(modelData)
                  }
                  onHovered: function(isHovered) { if (isHovered) root.setCursor("kinds", index) }
                }
              }
            }
          }

          // ---------- Silence ----------
          Column {
            visible: root.sections.indexOf("silence") >= 0
            width: parent.width
            spacing: Style.spacing.lg

            PanelSectionHeader {
              text: root.silenced
                ? (root.silenceUntilText !== "" ? "SILENCED UNTIL " + root.silenceUntilText : "SILENCED")
                : "SILENCE"
              foreground: root.foreground
              fontFamily: root.fontFamily
            }

            Row {
              id: silenceRow
              width: parent.width
              spacing: Style.spacing.md
              readonly property real cellWidth: (width - spacing * (root.silenceOptions.length - 1)) / root.silenceOptions.length

              Repeater {
                model: root.silenceOptions

                Button {
                  required property var modelData
                  required property int index
                  width: silenceRow.cellWidth
                  text: modelData.label
                  selected: root.silenced && modelData.minutes === 0 && root.sauron && root.sauron.silencedUntil < 0
                  hasCursor: root.cursorActive && root.section === "silence" && root.silenceIndex === index
                  bordered: true
                  foreground: root.foreground
                  fontFamily: root.fontFamily
                  fontSize: Style.font.bodySmall
                  onClicked: {
                    root.setCursor("silence", index)
                    root.chooseSilence(modelData)
                  }
                  onHovered: function(isHovered) { if (isHovered) root.setCursor("silence", index) }
                }
              }
            }
          }

          // ---------- Cameras ----------
          PanelSeparator {
            visible: root.online && root.cameras.length > 0
            foreground: root.foreground
          }

          Grid {
            id: grid
            visible: root.online && root.cameras.length > 0
            columns: root.columns
            spacing: root.gridGap

            Repeater {
              model: root.cameras
              delegate: CameraTile {}
            }
          }

          Text {
            visible: root.online && root.cameras.length === 0
            width: parent.width
            textFormat: Text.PlainText
            text: "No cameras are adopted in Protect yet."
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
          }

          Text {
            visible: root.sauron && root.sauron.liveError !== ""
            width: parent.width
            textFormat: Text.PlainText
            text: root.sauron ? root.sauron.liveError : ""
            color: root.urgent
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WordWrap
          }

          // ---------- Recent ----------
          PanelSeparator {
            visible: root.recent.length > 0
            foreground: root.foreground
          }

          Column {
            visible: root.recent.length > 0
            width: parent.width
            spacing: Style.spacing.xs

            PanelSectionHeader {
              text: "RECENT SIGHTINGS"
              foreground: root.foreground
              fontFamily: root.fontFamily
            }

            Repeater {
              model: root.recent
              delegate: EventRow {}
            }
          }
        }
      }
    }
  }

  // A camera's latest still with its name, detection badge, and state.
  component CameraTile: Item {
    id: tile
    required property var modelData
    required property int index

    readonly property var detections: root.activeByCamera[modelData.id] || []
    readonly property bool hot: detections.length > 0
    readonly property bool muted: root.sauron ? root.sauron.isMuted(modelData.id) : false
    readonly property bool hasCursor: root.cursorActive && root.section === "tiles" && root.tileIndex === index

    property real pulse: 1
    SequentialAnimation on pulse {
      running: tile.hot && root.eyeSeeing && root.opened && !Style.reduceMotion
      loops: Animation.Infinite
      onRunningChanged: if (!running) tile.pulse = 1
      NumberAnimation { to: 0.45; duration: 640; easing.type: Easing.InOutSine }
      NumberAnimation { to: 1; duration: 860; easing.type: Easing.InOutSine }
    }

    width: root.tileWidth
    height: Math.round(root.tileWidth * 9 / 16)
    onHasCursorChanged: if (hasCursor) root.scrollIntoView(tile)

    SnapshotImage {
      id: still
      anchors.fill: parent
      path: tile.modelData.snapshot
      seq: root.snapshots[tile.modelData.id] || 0
      opacity: tile.modelData.online ? 1 : 0.45
    }

    Rectangle {
      anchors.left: parent.left
      anchors.right: parent.right
      anchors.bottom: parent.bottom
      height: parent.height * 0.42
      radius: Style.cornerRadius
      gradient: Gradient {
        GradientStop { position: 0.0; color: Util.alpha(Color.popups.background, 0) }
        GradientStop { position: 1.0; color: Util.alpha(Color.popups.background, 0.88) }
      }
    }

    Text {
      id: nameLabel
      anchors.left: parent.left
      anchors.bottom: parent.bottom
      anchors.leftMargin: Style.space(8)
      anchors.bottomMargin: Style.space(6)
      width: Math.min(implicitWidth, parent.width - Style.space(22) - stateLabel.implicitWidth)
      textFormat: Text.PlainText
      text: tile.modelData.name
      color: Color.popups.text
      font.family: root.fontFamily
      font.pixelSize: Style.font.bodySmall
      font.bold: true
      elide: Text.ElideRight
    }

    Text {
      id: stateLabel
      anchors.left: nameLabel.right
      anchors.leftMargin: Style.space(6)
      anchors.baseline: nameLabel.baseline
      textFormat: Text.PlainText
      text: !tile.modelData.online ? "OFFLINE" : (tile.muted ? "MUTED" : "")
      color: Util.alpha(Color.popups.text, 0.7)
      font.family: root.fontFamily
      font.pixelSize: Style.font.caption
      font.bold: true
      font.letterSpacing: 1
    }

    Text {
      visible: tile.index < 9
      anchors.top: parent.top
      anchors.right: parent.right
      anchors.margins: Style.space(8)
      textFormat: Text.PlainText
      text: String(tile.index + 1)
      color: Util.alpha(Color.popups.text, 0.55)
      font.family: root.fontFamily
      font.pixelSize: Style.font.caption
      font.bold: true
    }

    BorderSurface {
      visible: tile.hot
      anchors.top: parent.top
      anchors.left: parent.left
      anchors.margins: Style.space(8)
      implicitWidth: badge.implicitWidth + Style.space(12)
      implicitHeight: badge.implicitHeight + Style.space(4)
      radius: Style.cornerRadius
      color: root.urgent
      borderSpec: Border.none()

      Text {
        id: badge
        anchors.centerIn: parent
        textFormat: Text.PlainText
        text: Kinds.describe(tile.detections).toUpperCase()
        color: Color.popups.background
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
        font.bold: true
        font.letterSpacing: 1
      }
    }

    BorderSurface {
      anchors.fill: parent
      color: "transparent"
      radius: Style.cornerRadius
      borderSpec: tile.hot
        ? Border.flat(root.urgent, Math.max(2, Style.space(2)))
        : Border.controlSpec("normal", root.foreground, Color.accent)
      opacity: tile.hot && root.eyeSeeing ? tile.pulse : 1
    }

    // Photos swallow the kit's subtle hover border, so the cursor gets a solid
    // accent ring inset over the still (an outset ring would be clipped).
    Rectangle {
      visible: tile.hasCursor
      anchors.fill: parent
      anchors.margins: tile.hot ? Math.max(2, Style.space(2)) : 0
      radius: Style.cornerRadius
      color: "transparent"
      border.width: Math.max(2, Style.space(2))
      border.color: Color.accent
    }

    MouseArea {
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      acceptedButtons: Qt.LeftButton | Qt.RightButton
      onEntered: root.setCursor("tiles", tile.index)
      onClicked: function(mouse) {
        if (!root.sauron) return
        if (mouse.button === Qt.RightButton) root.sauron.toggleMuted(tile.modelData.id)
        else root.showCamera(tile.modelData.id)
      }
    }
  }

  // One sighting: when, what, where. Click opens that camera live.
  component EventRow: CursorSurface {
    id: row
    required property var modelData
    required property int index

    readonly property bool ongoing: !modelData.end

    width: parent ? parent.width : 0
    implicitHeight: Style.spacing.popupRowHeight
    hasCursor: root.cursorActive && root.section === "events" && root.eventIndex === index
    foreground: root.foreground
    onHasCursorChanged: if (hasCursor) root.scrollIntoView(row)

    Rectangle {
      id: dot
      anchors.left: parent.left
      anchors.leftMargin: Style.spacing.rowPaddingX
      anchors.verticalCenter: parent.verticalCenter
      width: Style.space(6)
      height: width
      radius: width / 2
      color: row.ongoing ? root.urgent : root.dim
    }

    Text {
      id: when
      anchors.left: dot.right
      anchors.leftMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      textFormat: Text.PlainText
      text: Kinds.clock(row.modelData.start)
      color: root.dim
      font.family: root.fontFamily
      font.pixelSize: Style.font.bodySmall
    }

    Text {
      anchors.left: when.right
      anchors.leftMargin: Style.space(10)
      anchors.right: where.left
      anchors.rightMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      textFormat: Text.PlainText
      text: Kinds.describe(row.modelData.kinds)
      color: root.foreground
      font.family: root.fontFamily
      font.pixelSize: Style.font.bodySmall
      font.bold: row.ongoing
      elide: Text.ElideRight
    }

    Text {
      id: where
      anchors.right: parent.right
      anchors.rightMargin: Style.spacing.rowPaddingX
      anchors.verticalCenter: parent.verticalCenter
      textFormat: Text.PlainText
      text: root.sauron ? root.sauron.cameraName(row.modelData.camera) : ""
      color: root.dim
      font.family: root.fontFamily
      font.pixelSize: Style.font.bodySmall
    }

    MouseArea {
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      onEntered: root.setCursor("events", row.index)
      onClicked: root.showCamera(row.modelData.camera)
    }
  }
}
