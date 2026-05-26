import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  // The dev fleet binds to 0.0.0.0; without these entries Next's
  // same-origin check blocks the HMR websocket when the page is loaded
  // from a non-localhost host (LAN IPs, other dev machines).
  allowedDevOrigins: ["127.0.0.1", "localhost", "192.168.0.201", "192.168.64.1"],
};

export default nextConfig;

