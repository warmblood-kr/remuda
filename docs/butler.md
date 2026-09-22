# Butler

Butler is Remuda's local session manager. It starts and coordinates one agent
session through the same Lua runtime and Remuda protocol used by other
extensions. It works without Matrix configuration.

When Matrix credentials are configured, Butler can bridge one room: inbound
messages arrive through a small sync process and replies use the Butler MCP
tool. The bridge composes existing Remuda primitives; it does not add a
separate runtime or extension catalog.

Butler topics use stable session names and are delivered through the Butler
message queue. The current topic and workspace behavior is maintained in the
[design notes](design.md#butler-is-a-local-session-manager-with-an-optional-matrix-bridge).
