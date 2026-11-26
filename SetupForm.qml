import QtQuick
import qs.Commons
import qs.Ui

// Connecting to a console, inside the panel: find it, trust its certificate,
// paste an API key, and optionally add other addresses and instant live. The
// daemon checks each step against the console before anything is saved.
Column {
  id: form

  property var sauron: null
  property color foreground: Color.foreground
  property color dim: Qt.darker(foreground, 1.55)
  property color urgent: Color.urgent
  property string fontFamily: Style.font.family

  readonly property var loaded: sauron ? sauron.setupLoaded : null
  readonly property var probe: sauron ? sauron.setupProbe : null
  readonly property var saved: sauron ? sauron.setupSaved : null
  readonly property string busy: sauron ? sauron.setupBusy : ""

  // The probe answers for the address as it was typed; editing it starts over.
  readonly property bool found: probe !== null && probe.ok && probe.host === hostField.text.trim()
  readonly property var cert: found ? probe.cert : null
  property bool trusted: false
  readonly property bool ready: found && (cert === null || trusted)
  readonly property bool hasKey: loaded !== null && loaded.hasKey && loaded.configured
    && loaded.host === hostField.text.trim()
  property bool liveOn: false

  readonly property string errorField: saved && !saved.ok ? String(saved.field || "") : ""
  readonly property string errorText: saved && !saved.ok ? String(saved.message || "") : ""

  spacing: Style.spacing.lg

  function reset() {
    var l = loaded
    hostField.text = l ? String(l.host || "") : ""
    keyField.text = ""
    fallbackField.text = l && l.fallbacks ? l.fallbacks.join(", ") : ""
    userField.text = l ? String(l.username || "") : ""
    passwordField.text = ""
    liveOn = l ? String(l.username || "") !== "" : false
    trusted = false
    if (hostField.text !== "") findConsole()
    Qt.callLater(function() { (hostField.text === "" ? hostField : keyField).forceActiveFocus() })
  }

  function findConsole() {
    trusted = false
    if (sauron && hostField.text.trim() !== "") sauron.probeConsole(hostField.text.trim())
  }

  function save() {
    if (!sauron || !ready || busy !== "") return
    var fallbacks = fallbackField.text.split(",").map(function(a) { return a.trim() })
      .filter(function(a) { return a !== "" })
    sauron.saveSetup({
      host: hostField.text.trim(),
      pin: cert ? cert.sha256 : "",
      apiKey: keyField.text.trim(),
      fallbacks: fallbacks,
      username: liveOn ? userField.text.trim() : "",
      password: liveOn ? passwordField.text : ""
    })
  }

  onLoadedChanged: reset()
  // A certificate we already trust needs no second confirmation.
  onProbeChanged: if (found && cert && loaded && loaded.pin === cert.sha256 && !probe.changed) trusted = true

  component Hint: Text {
    width: parent ? parent.width : 0
    textFormat: Text.PlainText
    color: form.dim
    font.family: form.fontFamily
    font.pixelSize: Style.font.caption
    wrapMode: Text.WordWrap
  }

  component Problem: Text {
    width: parent ? parent.width : 0
    textFormat: Text.PlainText
    color: form.urgent
    font.family: form.fontFamily
    font.pixelSize: Style.font.bodySmall
    wrapMode: Text.WordWrap
  }

  component Field: TextField {
    width: parent ? parent.width : 0
    foreground: form.foreground
    font.family: form.fontFamily
    font.pixelSize: Style.font.bodySmall
    onAccepted: form.save()
  }

  // ---------- Console ----------
  PanelSectionHeader {
    text: "CONSOLE"
    foreground: form.foreground
    fontFamily: form.fontFamily
  }

  Row {
    width: parent.width
    spacing: Style.spacing.md

    Field {
      id: hostField
      width: parent.width - findButton.width - parent.spacing
      placeholderText: "IP or hostname, e.g. 192.168.1.1"
      onAccepted: form.findConsole()
    }

    Button {
      id: findButton
      text: form.busy === "probe" ? "Looking…" : "Find"
      bordered: true
      foreground: form.foreground
      fontFamily: form.fontFamily
      fontSize: Style.font.bodySmall
      onClicked: form.findConsole()
    }
  }

  Problem {
    visible: form.probe !== null && !form.probe.ok && form.busy !== "probe"
    text: form.probe && !form.probe.ok ? String(form.probe.message || "") : ""
  }

  Hint {
    visible: form.found
    text: !form.found ? "" : (form.probe.console ? "UniFi console found" : String(form.probe.message || ""))
      + (form.probe.plain ? " · plain http has no TLS: for testing only" : "")
      + (form.found && form.cert === null && !form.probe.plain ? " · certificate trusted" : "")
  }

  // ---------- Certificate ----------
  Column {
    visible: form.cert !== null
    width: parent.width
    spacing: Style.spacing.md

    Problem {
      visible: form.found && form.probe.changed
      text: "The console's certificate changed. Only trust it on your home network."
    }

    Text {
      width: parent.width
      textFormat: Text.PlainText
      text: form.cert
        ? "Certificate SHA256 " + form.cert.short + " (" + (form.cert.selfSigned ? "self-signed" : "not publicly trusted")
          + (form.cert.cn ? ", CN=" + form.cert.cn : "") + ")"
        : ""
      color: form.foreground
      font.family: form.fontFamily
      font.pixelSize: Style.font.bodySmall
      wrapMode: Text.WrapAnywhere
    }

    Row {
      spacing: Style.spacing.md

      ToggleSwitch {
        checked: form.trusted
        foreground: form.foreground
        onToggled: form.trusted = !form.trusted
      }

      Text {
        anchors.verticalCenter: parent.verticalCenter
        textFormat: Text.PlainText
        text: "Trust this console"
        color: form.foreground
        font.family: form.fontFamily
        font.pixelSize: Style.font.bodySmall
      }
    }

    Hint {
      text: "Sauron will only talk to the machine holding this certificate, at every address."
    }
  }

  // ---------- API key ----------
  Column {
    visible: form.ready
    width: parent.width
    spacing: Style.spacing.md

    PanelSectionHeader {
      text: "API KEY"
      foreground: form.foreground
      fontFamily: form.fontFamily
    }

    Field {
      id: keyField
      password: true
      placeholderText: form.hasKey ? "Leave empty to keep the current key" : "Paste the key"
    }

    Hint { text: form.loaded ? String(form.loaded.keyHint || "") : "" }
    Problem { visible: form.errorField === "key"; text: form.errorText }
  }

  // ---------- Other addresses ----------
  Column {
    visible: form.ready
    width: parent.width
    spacing: Style.spacing.md

    PanelSectionHeader {
      text: "OTHER ADDRESSES"
      foreground: form.foreground
      fontFamily: form.fontFamily
    }

    Field {
      id: fallbackField
      placeholderText: "Optional, e.g. unifi.tail1234.ts.net"
    }

    Hint { text: "Tried in order when the console doesn't answer at its address, e.g. away from home over Tailscale. Comma-separated." }
    Problem { visible: form.errorField === "fallbacks"; text: form.errorText }
  }

  // ---------- Instant live ----------
  Column {
    visible: form.ready
    width: parent.width
    spacing: Style.spacing.md

    Row {
      width: parent.width
      spacing: Style.spacing.md

      ToggleSwitch {
        checked: form.liveOn
        foreground: form.foreground
        onToggled: form.liveOn = !form.liveOn
      }

      Text {
        anchors.verticalCenter: parent.verticalCenter
        textFormat: Text.PlainText
        text: "Instant live video"
        color: form.foreground
        font.family: form.fontFamily
        font.pixelSize: Style.font.bodySmall
      }
    }

    Hint { text: form.loaded ? String(form.loaded.instantHint || "") : "" }

    Field {
      id: userField
      visible: form.liveOn
      placeholderText: "UniFi username (local-only user)"
    }

    Field {
      id: passwordField
      visible: form.liveOn
      password: true
      placeholderText: form.loaded && form.loaded.hasPassword && userField.text.trim() === form.loaded.username
        ? "Leave empty to keep the saved password" : "Password"
    }

    Problem { visible: form.errorField === "live"; text: form.errorText }
  }

  Problem {
    visible: form.errorText !== "" && (form.errorField === "host" || form.errorField === "")
    text: form.errorText
  }

  Row {
    spacing: Style.spacing.md

    Button {
      text: form.busy === "save" ? "Checking…" : "Save"
      selected: form.ready
      bordered: true
      enabled: form.ready && form.busy === ""
      opacity: enabled ? 1 : 0.5
      foreground: form.foreground
      fontFamily: form.fontFamily
      fontSize: Style.font.bodySmall
      horizontalPadding: Style.space(18)
      onClicked: form.save()
    }

    Button {
      visible: form.loaded !== null && form.loaded.configured
      text: "Cancel"
      bordered: true
      foreground: form.foreground
      fontFamily: form.fontFamily
      fontSize: Style.font.bodySmall
      onClicked: if (form.sauron) form.sauron.closeSetup()
    }
  }
}
