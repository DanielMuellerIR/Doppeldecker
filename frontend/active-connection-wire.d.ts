import type { RemoteProvider } from "./provider-command-names.js";

export interface ActiveConnectionWire {
  protocol: RemoteProvider;
  profile_id: string;
  revision: number;
}
