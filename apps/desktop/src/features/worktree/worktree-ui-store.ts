import { create } from "zustand";

/** The one worktree dialog that is open, if any (ADR 0326). */
export type WorktreeDialog =
  | { kind: "merge"; sessionId: string; title: string }
  | { kind: "setupLog"; sessionId: string; title: string }
  | { kind: "setupCommand"; projectPath: string; projectName: string };

type WorktreeUiState = {
  dialog: WorktreeDialog | null;
  open: (dialog: WorktreeDialog) => void;
  close: () => void;
};

/**
 * Dialog state shared by the sidebar menus that open a worktree dialog and the
 * host that renders it. Kept out of the central app store on purpose.
 */
export const useWorktreeUi = create<WorktreeUiState>((set) => ({
  dialog: null,
  open: (dialog) => set({ dialog }),
  close: () => set({ dialog: null }),
}));
