// @ts-check

import { invoke } from "./tauri-api.js";

/** @typedef {{path: string, showHidden: boolean}} ScanDirRequest */

/**
 * @typedef {object} DirectoryEntry
 * @property {string} name
 * @property {boolean} is_dir
 * @property {boolean} is_file
 * @property {boolean} is_symlink
 * @property {number} size
 * @property {number | null} mtime
 * @property {number | null} created
 * @property {string | null} owner
 * @property {string | null} group
 * @property {string | null} extension
 * @property {number | null} mode
 * @property {number | null} device
 * @property {number | null} inode
 * @property {number | null} hard_links
 */

/** @typedef {DirectoryEntry & {safely_decoded: boolean, is_hidden: boolean}} DirectoryDisplayEntry */

/** @typedef {{path: string, parent: string | null, entries: DirectoryDisplayEntry[]}} ScanDirListing */

/**
 * @param {ScanDirRequest} request
 * @returns {Promise<ScanDirListing>}
 */
export function scanDir(request) {
  return invoke("scan_dir", /** @satisfies {ScanDirRequest} */ ({
    path: request.path,
    showHidden: request.showHidden,
  }));
}
