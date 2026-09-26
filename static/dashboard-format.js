export function fmtTime(iso) {
  if (!iso) return "-";
  var d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toLocaleString();
}

export function shortId(id) {
  return typeof id === "string" && id.length > 12 ? id.slice(0, 8) + "..." : id;
}
