.pragma library

// Detection kinds as normalized by the sauron daemon, in display order.
var order = ["person", "vehicle", "animal", "package", "face", "licensePlate", "ring", "audio", "motion"]

var labels = {
  person: "Person",
  vehicle: "Vehicle",
  animal: "Animal",
  package: "Package",
  face: "Face",
  licensePlate: "Plate",
  ring: "Doorbell",
  audio: "Sound",
  motion: "Motion"
}

var defaultNotify = ["person", "vehicle", "package", "ring"]

function label(kind) {
  return labels[kind] || String(kind || "")
}

function describe(kinds) {
  var list = kinds instanceof Array ? kinds : []
  var sorted = list.slice().sort(function(a, b) {
    var ia = order.indexOf(a), ib = order.indexOf(b)
    return (ia < 0 ? order.length : ia) - (ib < 0 ? order.length : ib)
  })
  var out = []
  for (var i = 0; i < sorted.length; i++) out.push(label(sorted[i]))
  return out.join(", ")
}

// Kinds any camera can produce, in display order; every kind when the camera
// list is still empty so the filter row never collapses while connecting.
function available(cameras) {
  var seen = {}
  var list = cameras instanceof Array ? cameras : []
  for (var i = 0; i < list.length; i++) {
    var kinds = list[i] && list[i].kinds instanceof Array ? list[i].kinds : []
    for (var j = 0; j < kinds.length; j++) seen[kinds[j]] = true
  }
  if (list.length === 0) return order.slice()
  var out = []
  for (var k = 0; k < order.length; k++) if (seen[order[k]]) out.push(order[k])
  return out
}

function clock(ms) {
  if (!ms) return ""
  var d = new Date(ms)
  var h = d.getHours(), m = d.getMinutes()
  return (h < 10 ? "0" : "") + h + ":" + (m < 10 ? "0" : "") + m
}

// Newest sighting first by when it started, not when it arrived: Protect reports
// some detections late (sound; packages only once they have ended).
function addSighting(list, ev, limit) {
  var out = list.slice()
  var i = 0
  while (i < out.length && out[i].start > ev.start) i++
  out.splice(i, 0, ev)
  if (out.length > limit) out.length = limit
  return out
}
