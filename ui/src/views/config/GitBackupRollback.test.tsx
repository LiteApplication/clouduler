import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ConfirmProvider, ToastProvider } from "../../ds";
import { GitBackupRollback } from "./sections/GitBackupRollback";

const auth = vi.hoisted(() => ({ isAdmin: true }));
vi.mock("../../auth/AuthContext", () => ({ useAuth: () => auth }));

const api = vi.hoisted(() => ({
  gitBackupCommits: vi.fn(),
  gitBackupRollback: vi.fn(),
  gitBackupReset: vi.fn(),
  selfUpdateReleases: vi.fn(),
}));
vi.mock("../../api/client", () => ({
  ApiError: class extends Error {
    status = 409;
  },
  api,
}));

const commits = [
  {
    sha: "a".repeat(40),
    short: "a".repeat(12),
    message: "update stack web",
    author: "clouduler",
    time: 1,
  },
  {
    sha: "b".repeat(40),
    short: "b".repeat(12),
    message: "update cluster.yaml",
    author: "clouduler",
    time: 2,
  },
];

function renderIt(configured = true) {
  return render(
    <ToastProvider>
      <ConfirmProvider>
        <GitBackupRollback configured={configured} />
      </ConfirmProvider>
    </ToastProvider>,
  );
}

describe("GitBackupRollback", () => {
  beforeEach(() => {
    auth.isAdmin = true;
    Object.values(api).forEach((m) => m.mockReset());
    api.gitBackupCommits.mockResolvedValue(commits);
    api.selfUpdateReleases.mockResolvedValue({ tags: ["v0.19.0", "v0.18.0"] });
  });

  it("renders nothing when git backup is off or the user is not admin", () => {
    const a = renderIt(false);
    expect(screen.queryByText(/Advanced: roll back/)).toBeNull();
    a.unmount();
    auth.isAdmin = false;
    const b = renderIt(true);
    expect(screen.queryByText(/Advanced: roll back/)).toBeNull();
    b.unmount();
  });

  it("only loads commits once opened", async () => {
    renderIt();
    expect(api.gitBackupCommits).not.toHaveBeenCalled();
    await userEvent.click(screen.getByText(/Advanced: roll back/));
    expect(await screen.findByText("update stack web")).toBeInTheDocument();
    expect(api.gitBackupCommits).toHaveBeenCalledTimes(1);
  });

  it("requires typing the short sha, then rolls back", async () => {
    api.gitBackupRollback.mockResolvedValue({ commit: "c".repeat(40) });
    renderIt();
    await userEvent.click(screen.getByText(/Advanced: roll back/));
    await userEvent.click(
      (await screen.findAllByRole("button", { name: /Roll back to/ }))[0],
    );
    await userEvent.type(await screen.findByRole("textbox"), "a".repeat(12));
    await userEvent.click(
      await screen.findByRole("button", { name: "Roll back" }),
    );
    expect(api.gitBackupRollback).toHaveBeenCalledWith("a".repeat(40));
    expect(
      await screen.findByText(/new commit cccccccccccc/),
    ).toBeInTheDocument();
  });

  it("shows the server message when the rollback is rejected", async () => {
    const { ApiError } = await import("../../api/client");
    api.gitBackupRollback.mockRejectedValue(
      new (ApiError as unknown as new (m: string) => Error)(
        "pull rejected: bad yaml",
      ),
    );
    renderIt();
    await userEvent.click(screen.getByText(/Advanced: roll back/));
    await userEvent.click(
      (await screen.findAllByRole("button", { name: /Roll back to/ }))[1],
    );
    await userEvent.type(await screen.findByRole("textbox"), "b".repeat(12));
    await userEvent.click(
      await screen.findByRole("button", { name: "Roll back" }),
    );
    expect(
      await screen.findByText(/pull rejected: bad yaml/),
    ).toBeInTheDocument();
  });

  describe("reset to commit", () => {
    async function openReset() {
      renderIt();
      await userEvent.click(screen.getByText(/Advanced: roll back/));
      await userEvent.click(
        await screen.findByRole("button", { name: /Reset to commit/ }),
      );
    }

    it("is not shown until the reset method is chosen", async () => {
      renderIt();
      await userEvent.click(screen.getByText(/Advanced: roll back/));
      expect(screen.queryByLabelText("Config commit hash")).toBeNull();
    });

    it("validates the hash", async () => {
      await openReset();
      const btn = screen.getByRole("button", { name: "Reset to this commit" });
      expect(btn).toBeDisabled();
      await userEvent.type(screen.getByLabelText("Config commit hash"), "abz");
      expect(btn).toBeDisabled();
      expect(screen.getByText(/4 to 40 hexadecimal/)).toBeInTheDocument();
      await userEvent.clear(screen.getByLabelText("Config commit hash"));
      await userEvent.type(screen.getByLabelText("Config commit hash"), "abcd");
      expect(btn).toBeEnabled();
    });

    async function submit(hash: string, expected: unknown, tagText?: RegExp) {
      api.gitBackupReset.mockResolvedValue({
        commit: "a".repeat(40),
        branch: "rolled-back/x-aaaa",
        release_tag: null,
        release_source: "current",
      });
      await userEvent.click(
        screen.getByRole("button", { name: "Reset to this commit" }),
      );
      if (tagText) expect((await screen.findAllByText(tagText)).length).toBeGreaterThan(1);
      await userEvent.type((await screen.findAllByRole("textbox")).at(-1)!, hash.slice(0, 12));
      await userEvent.click(
        await screen.findByRole("button", { name: "Reset and reboot all nodes" }),
      );
      expect(api.gitBackupReset).toHaveBeenCalledWith(hash, expected);
    }

    it("sends auto by default", async () => {
      await openReset();
      await userEvent.type(screen.getByLabelText("Config commit hash"), "a".repeat(12));
      await submit("a".repeat(12), { mode: "auto" }, /recorded at that commit/);
      expect(await screen.findByText(/rolled-back\/x-aaaa/)).toBeInTheDocument();
    });

    it("sends keep-current", async () => {
      await openReset();
      await userEvent.type(screen.getByLabelText("Config commit hash"), "a".repeat(12));
      await userEvent.selectOptions(screen.getByLabelText("Release"), "current");
      await submit("a".repeat(12), { mode: "current" }, /nothing is downloaded/);
    });

    it("sends a tag picked from the list, and commit and release change independently", async () => {
      await openReset();
      await userEvent.type(screen.getByLabelText("Config commit hash"), "a".repeat(12));
      await screen.findByRole("option", { name: "v0.18.0" });
      await userEvent.selectOptions(screen.getByLabelText("Release"), "v0.18.0");
      await userEvent.clear(screen.getByLabelText("Config commit hash"));
      await userEvent.type(screen.getByLabelText("Config commit hash"), "c".repeat(8));
      expect(screen.getByLabelText("Release")).toHaveValue("v0.18.0");
      await submit("c".repeat(8), { mode: "tag", tag: "v0.18.0" }, /release v0.18.0/);
    });

    it("lets a custom tag be typed when the list fails to load", async () => {
      api.selfUpdateReleases.mockRejectedValue(new Error("no"));
      await openReset();
      await userEvent.type(screen.getByLabelText("Config commit hash"), "a".repeat(12));
      expect(await screen.findByText(/Could not load the release list/)).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Reset to this commit" })).toBeDisabled();
      await userEvent.type(screen.getByLabelText("Release tag"), "v0.1.0");
      await submit("a".repeat(12), { mode: "tag", tag: "v0.1.0" });
    });

    it("fills the hash from the commit list and shows server errors", async () => {
      const { ApiError } = await import("../../api/client");
      api.gitBackupReset.mockRejectedValue(
        new (ApiError as unknown as new (m: string) => Error)("push rejected"),
      );
      await openReset();
      await userEvent.click(
        (await screen.findAllByRole("button", { name: /^Use / }))[1],
      );
      expect(screen.getByLabelText("Config commit hash")).toHaveValue(
        "b".repeat(40),
      );
      await userEvent.click(
        screen.getByRole("button", { name: "Reset to this commit" }),
      );
      await userEvent.type(
        (await screen.findAllByRole("textbox")).at(-1)!,
        "b".repeat(12),
      );
      await userEvent.click(
        await screen.findByRole("button", {
          name: "Reset and reboot all nodes",
        }),
      );
      expect(api.gitBackupReset).toHaveBeenCalledWith("b".repeat(40), { mode: "auto" });
      expect(await screen.findByText(/push rejected/)).toBeInTheDocument();
    });
  });
});
