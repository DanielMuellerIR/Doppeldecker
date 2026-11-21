// @ts-check

import { invoke } from "./tauri-api.js";

/** @param {import("./file-property-wire.js").FilePropertyReadRequest} request */
export const readFinderTags = request => invoke("get_file_finder_tags", { request });

/** @param {import("./file-property-wire.js").FinderTagEditRequest} request */
export const editFinderTags = request => invoke("change_file_finder_tags", { request });

/** @param {import("./file-property-wire.js").FinderCommentReadRequest} request */
export const readFinderComments = request => invoke("get_file_finder_comments", { request });

/** @param {import("./file-property-wire.js").FinderCommentEditRequest} request */
export const editFinderComments = request => invoke("change_file_finder_comments", { request });

/** @param {import("./file-property-wire.js").FileFlagReadRequest} request */
export const readFileFlags = request => invoke("get_file_flags", { request });

/** @param {import("./file-property-wire.js").FileFlagEditRequest} request */
export const editFileFlag = request => invoke("change_file_flag", { request });

/** @param {import("./file-property-wire.js").FilePropertyReadRequest} request */
export const readVisibility = request => invoke("get_file_visibility", { request });

/** @param {import("./file-property-wire.js").VisibilityEditRequest} request */
export const editVisibility = request => invoke("change_file_visibility", { request });
