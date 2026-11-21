import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const fixture = JSON.parse(readFileSync(new URL("./file-property-wire.json", import.meta.url), "utf8"));

export function assertFilePropertyWire(feature, read, edit) {
  assert.deepEqual(read, fixture[feature].read);
  assert.match(edit.request.requestId, /^[a-f0-9-]{36}$/);
  assert.deepEqual({ ...edit, request: { ...edit.request, requestId: "bound" } }, fixture[feature].edit);
}
