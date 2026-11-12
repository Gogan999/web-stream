// What the host says it can do (the `capabilities` control message), so the page shows only
// controls that work. Until the host speaks, nothing optional is offered.

export const NO_CAPABILITIES = Object.freeze({ clipboard: false, sas: false, hostName: '', version: '' });

/** The message's fields, with anything missing or of the wrong type switched off. */
export function parseCapabilities(msg) {
  if (!msg || msg.type !== 'capabilities') return { ...NO_CAPABILITIES };
  return {
    clipboard: msg.clipboard === true,
    sas: msg.sas === true,
    hostName: typeof msg.hostName === 'string' ? msg.hostName.slice(0, 64) : '',
    version: typeof msg.version === 'string' ? msg.version.slice(0, 32) : '',
  };
}

/** Shows or hides the Ctrl+Alt+Del buttons (they only work with the service) and names the page. */
export function applyCapabilities(caps, { sasButtons, doc }) {
  for (const el of sasButtons) el.hidden = !caps.sas;
  if (doc) doc.title = caps.hostName ? `${caps.hostName} - web-stream` : 'web-stream';
}
