export interface NameSearchStartedWire {
  run_id: number;
}

export interface DiscoveredNetworkServiceWire {
  protocol: "smb" | "sftp" | "webdav";
  name: string;
  host: string;
  port: number;
}

export interface CliInstallStatusWire {
  state: "installed" | "offer" | "declined" | "conflict" | "outside_applications" | "unavailable";
}
