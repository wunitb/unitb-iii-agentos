import { useState } from "react";
import SectionHeader from "./SectionHeader";

const STEPS = [
          { label: "Clone UnitB AgentOS", cmd: "git clone https://github.com/wunitb/unitb-iii-agentos && cd unitb-iii-agentos" },
          { label: "Build iii v0.23.0 in Podman or Docker", cmd: "bash scripts/oci-stack.sh build" },
          { label: "Start the private OCI runtime", cmd: "bash scripts/oci-stack.sh up" },
          { label: "Configure your provider interactively", cmd: "bash scripts/oci-stack.sh exec agentos onboard" },
          { label: "Restart to load provider settings", cmd: "bash scripts/oci-stack.sh stop && bash scripts/oci-stack.sh up" },
          { label: "Create an agent and its capability document", cmd: "bash scripts/oci-stack.sh exec agentos agent new assistant" },
          { label: "Open the terminal UI", cmd: "bash scripts/oci-stack.sh exec agentos tui" },
          { label: "Inspect assigned endpoints and stop owned containers", cmd: "bash scripts/oci-stack.sh status && bash scripts/oci-stack.sh stop" },
        ];

export default function Install() {
  const [copied, setCopied] = useState<number | null>(null);

  async function copy(i: number, cmd: string) {
    try {
      await navigator.clipboard.writeText(cmd);
      setCopied(i);
      setTimeout(() => setCopied(null), 1400);
    } catch {
      /* no-op */
    }
  }

  return (
    <section id="install" className="py-24 border-b border-line">
      <div className="mx-auto px-6" style={{ maxWidth: "min(1240px, 92vw)" }}>
        <SectionHeader num="11" label="Install" />

        <h2 className="h-display text-[36px] md:text-[48px] mb-12 max-w-[20ch]">
          Build once. <em>Run through OCI.</em>
        </h2>

        <ol className="border-t border-l border-line">
          {STEPS.map((s, i) => (
            <li key={i} className="grid grid-cols-[40px_1fr_auto] border-r border-b border-line items-center">
              <div className="self-stretch flex items-center justify-center border-r border-line py-4 font-mono text-[12px] text-fg-3">
                {String(i + 1).padStart(2, "0")}
              </div>
              <div className="px-5 py-4">
                <div className="eyebrow mb-1.5">{s.label}</div>
                <code className="font-mono text-[12.5px] text-fg break-all">{s.cmd}</code>
              </div>
              <button
                onClick={() => copy(i, s.cmd)}
                className="font-mono text-[10.5px] tracking-[0.18em] uppercase text-fg-3 hover:text-fg px-4 py-2 border-l border-line self-stretch"
              >
                {copied === i ? "copied" : "copy"}
              </button>
            </li>
          ))}
        </ol>

        <p className="mt-5 max-w-[80ch] font-mono text-[11px] text-fg-3">
Podman or Docker runs Linux containers on Linux or macOS. Current runtime proof
            covers Linux AArch64; other hosts need their own verification. Keep provider
            credentials in the private AGENTOS_OCI_HOME, never in the checkout.
        </p>

        <div className="mt-8 grid sm:grid-cols-3 gap-6 text-[12.5px] text-fg-3 font-mono">
          <div>
            <div className="eyebrow mb-1">Engine WS</div>
            Authenticated · endpoint from status
          </div>
          <div>
            <div className="eyebrow mb-1">HTTP triggers</div>
            Host loopback · dynamic port
          </div>
          <div>
            <div className="eyebrow mb-1">Streams</div>
            Private to the container
          </div>
        </div>
      </div>
    </section>
  );
}
