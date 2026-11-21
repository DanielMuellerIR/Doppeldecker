export interface SystemMountWire {
  id: string;
  name: string;
  path: string | null;
  kind: "internal" | "external" | "network" | "volume";
  filesystem: string;
  total_bytes: number;
  available_bytes: number;
  space_error?: { code: string; message: string };
  read_only: boolean;
  ejectable: boolean;
}
