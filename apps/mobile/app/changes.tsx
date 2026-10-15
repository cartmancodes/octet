import { FlashList, type ListRenderItem } from "@shopify/flash-list";
import { useFocusEffect, useIsFocused, useLocalSearchParams, useRouter } from "expo-router";
import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Pressable, StyleSheet, Text, View } from "react-native";
import { useSafeAreaInsets } from "react-native-safe-area-context";

import {
  BINARY_NOTE,
  CLIPPED_NOTE,
  NO_TESTS,
  branchLines,
  changeSummary,
  diffLines,
  lastChecks,
  matchFile,
  type DiffLine,
} from "../src/changes";
import type { ChangesAnswer, ListedFile } from "../src/client/answers";
import { Refusal } from "../src/client/HubClient";
import { DiffLineRow, DiffNote, FileRow, SIDE } from "../src/components/FileRow";
import { HubBanner } from "../src/components/HubBanner";
import { failure, useAlive, useSend, type Failure } from "../src/components/send";
import { useHubClient } from "../src/hub";
import { FIELD_CHARS, field } from "../src/model/text";
import type { DiffFrame, FileChange } from "../src/protocol";
import { sessionName } from "../src/sessions";
import { octetStore, unwatch, useEntries, useHub, useSessionHead, watch } from "../src/store";
import { dark, light, radius, spacing, type, useTheme, type Theme } from "../src/theme";
import { Button, Card } from "../src/ui";

/** The hub's refusal of a workspace that is not in a repository, `unavailable` with these words
 * (`not_a_repository`, crates/octet-hub/src/git.rs): the screen's empty state, not a failure. */
export const NOT_A_REPOSITORY = "This folder isn't a git repository";
const NOT_A_REPOSITORY_CODE = "unavailable";
/** Under a file opened while the listing failed: no diff can be asked for until one is in. */
export const ASK_AGAIN = "Ask for the changes again";
export const LOADING = "Loading changes…";
export const LOADING_DIFF = "Loading the diff…";
export const NO_CHANGES = "Nothing changed since the last commit";
const WRONG: Failure = { text: "Something went wrong. Try again.", retry: true };

/** What a diff was of: the listed file's lines and status when it was asked for. */
type Stamp = Pick<FileChange, "add" | "del" | "status">;
const sameStamp = (a: Stamp, b: Stamp | undefined): boolean =>
  b !== undefined && a.add === b.add && a.del === b.del && a.status === b.status;

/** A file's diff, by its raw path, kept for the screen's life: a pull forgets them all, and any
 * other reload the ones of files that changed since. `truncated`: lines past what is drawn. */
type Diff =
  | { state: "loading" }
  | { state: "ready"; of: Stamp; lines: DiffLine[]; clipped: boolean; truncated: boolean; binary: boolean }
  | { state: "failed"; text: string };

/** One row of the list: a file, a line of an open file's diff, or a note under an open file.
 * Keys are the raw path's: two raw paths can clean to the same text. */
type Item =
  | { kind: "file"; key: string; file: ListedFile; open: boolean }
  | { kind: "line"; key: string; line: DiffLine }
  | { kind: "note"; key: string; text: string; words: boolean };

interface Listing {
  /** `loading` until the first answer; `norepo` when the workspace is not in a repository. */
  state: "loading" | "ready" | "norepo";
  frame: ChangesAnswer | null;
  /** Whether a load that follows an earlier one is out (the pull spinner). */
  pulling: boolean;
  failed: Failure | null;
}

const NO_FILES: ListedFile[] = [];
const NONE: ReadonlySet<string> = new Set();
const NO_DIFFS: ReadonlyMap<string, Diff> = new Map();
const keyOf = (item: Item): string => item.key;
const typeOf = (item: Item): string => item.kind;

const isChanges = (x: unknown): x is ChangesAnswer => typeof x === "object" && x !== null && (x as { type?: unknown }).type === "changes";
const isDiff = (x: unknown): x is DiffFrame => typeof x === "object" && x !== null && (x as { type?: unknown }).type === "diff";

/**
 * Changes, opened from a file-edit tool entry's "view ›" (`/changes?id=<session>&file=<path>`, that
 * file open) or from Usage's or Session info's "Changes ›": what the session's workspace changed
 * against HEAD, each file's diff on a tap, the last test run (Checks) and the branch.
 *
 * The hub lets a phone ask for a diff only of a path its last `changes` answer on this connection
 * listed, exactly as listed, so the screen asks for the listing whenever the connection is (again)
 * ready, asks for diffs only once it is in, and asks by the raw path the client kept. Everything
 * the hub sent is drawn cleaned, as plain text; the raw path never is.
 */
export default function Changes() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const params = useLocalSearchParams<{ id?: string | string[]; file?: string | string[] }>();
  const id = typeof params.id === "string" ? params.id : "";
  const file = typeof params.file === "string" ? field(params.file, FIELD_CHARS) : "";
  const client = useHubClient();
  const hub = useHub();
  const focused = useIsFocused();
  const head = useSessionHead(id);
  const send = useSend();
  const alive = useAlive();

  // The session streams while this screen is in front, as it does for Usage: Checks is live.
  useFocusEffect(
    useCallback(() => {
      if (!client || id === "") return;
      if (octetStore.getState().bySession.get(id)?.ended) return;
      if (octetStore.getState().sessions.every((x) => x.id !== id)) return; // not in the list: the hub would refuse
      watch(octetStore, client, id);
      return () => unwatch(client, id);
    }, [client, id]),
  );

  const [lastWorkspace, setLastWorkspace] = useState(head.workspace);
  if (head.workspace !== undefined && head.workspace !== lastWorkspace) setLastWorkspace(head.workspace);
  const name = sessionName(lastWorkspace);

  const [listing, setListing] = useState<Listing>({ state: "loading", frame: null, pulling: false, failed: null });
  // The open files and the diffs, with refs for the answers that come later to read.
  const [expanded, setExpandedState] = useState<ReadonlySet<string>>(NONE);
  const [diffs, setDiffsState] = useState<ReadonlyMap<string, Diff>>(NO_DIFFS);
  const expandedRef = useRef(expanded);
  const diffsRef = useRef(diffs);
  const setExpanded = useCallback((next: ReadonlySet<string>) => {
    expandedRef.current = next;
    setExpandedState(next);
  }, []);
  const setDiff = useCallback((path: string, diff: Diff | null) => {
    const next = new Map(diffsRef.current);
    if (diff === null) next.delete(path);
    else next.set(path, diff);
    diffsRef.current = next;
    setDiffsState(next);
  }, []);
  const keepDiffs = useCallback((keep: (diff: Diff, raw: string) => boolean) => {
    const next = new Map([...diffsRef.current].filter(([raw, d]) => keep(d, raw)));
    diffsRef.current = next;
    setDiffsState(next);
  }, []);
  /** The newest `changes` request; an answer or refusal to an older one is dropped (the hub's
   * "Superseded; ask again" among them). */
  const loadId = useRef(0);
  /** The newest listing came on this connection: diffs may be asked for. */
  const fresh = useRef(false);
  /** The route's file was looked for in a listing. */
  const routed = useRef(false);
  /** The newest listing's files, by raw path. */
  const listed = useRef<ReadonlyMap<string, ListedFile>>(new Map());

  /** Asks for the diff of the listed file whose raw path is `raw`, by that path. */
  const askDiff = useCallback(
    (raw: string) => {
      const under = loadId.current;
      const of = listed.current.get(raw);
      if (of === undefined) return;
      const stamp: Stamp = { add: of.add, del: of.del, status: of.status };
      setDiff(raw, { state: "loading" });
      send({ type: "diff", session: id, path: raw }).then(
        (frame) => {
          if (!alive() || loadId.current !== under) return;
          if (!isDiff(frame)) {
            setDiff(raw, { state: "failed", text: WRONG.text });
            return;
          }
          const { lines, truncated } = diffLines(frame.text);
          setDiff(raw, { state: "ready", of: stamp, lines, clipped: frame.clipped, truncated, binary: frame.binary });
        },
        (error: unknown) => {
          if (!alive() || loadId.current !== under) return;
          setDiff(raw, { state: "failed", text: failure(error).text });
        },
      );
    },
    [send, id, alive, setDiff],
  );

  // Asks the Mac for the listing. Reading is safe to repeat, so a failure offers Try again. A
  // pull forgets every diff; any reload forgets the ones not in, which are asked for again (the
  // open ones) once the listing is.
  const load = useCallback(
    (pull: boolean) => {
      const mine = ++loadId.current;
      fresh.current = false;
      keepDiffs((d) => !pull && d.state === "ready");
      setListing((l) => ({ ...l, pulling: pull && l.state === "ready", failed: null }));
      send({ type: "changes", session: id }).then(
        (frame) => {
          if (!alive() || loadId.current !== mine) return;
          if (!isChanges(frame)) {
            setListing((l) => ({ ...l, pulling: false, failed: WRONG }));
            return;
          }
          const byRaw = new Map(frame.files.map((f) => [f.raw, f]));
          listed.current = byRaw;
          // A kept diff of a file whose lines or status changed is of an older file.
          keepDiffs((d, raw) => d.state === "ready" && sameStamp(d.of, byRaw.get(raw)));
          const open = new Set([...expandedRef.current].filter((raw) => byRaw.has(raw)));
          if (!routed.current) {
            routed.current = true;
            const named = file === "" ? null : matchFile(frame.files.map((f) => f.path), file);
            const hits = named === null ? [] : frame.files.filter((f) => f.path === named);
            if (hits.length === 1 && hits[0]) open.add(hits[0].raw);
          }
          fresh.current = true;
          setExpanded(open);
          setListing({ state: "ready", frame, pulling: false, failed: null });
          for (const raw of open) if (diffsRef.current.get(raw)?.state !== "ready") askDiff(raw);
        },
        (error: unknown) => {
          if (!alive() || loadId.current !== mine) return;
          if (error instanceof Refusal && error.code === NOT_A_REPOSITORY_CODE && error.message === NOT_A_REPOSITORY) {
            setListing({ state: "norepo", frame: null, pulling: false, failed: null });
            return;
          }
          setListing((l) => ({ ...l, pulling: false, failed: failure(error) }));
        },
      );
    },
    [send, id, alive, file, askDiff, keepDiffs, setExpanded],
  );
  // On open, and again whenever the connection comes back: the hub forgot the listing with the
  // old connection, and the workspace may have changed meanwhile.
  useEffect(() => {
    if (hub !== "ready") {
      fresh.current = false;
      return;
    }
    // eslint-disable-next-line react-hooks/set-state-in-effect -- one extra render on a reconnect, to clear a stale failure now
    if (id !== "") load(false);
  }, [hub, id, load]);
  const pull = useCallback(() => load(true), [load]);
  const retry = useCallback(() => load(false), [load]);

  // A tap opens a file (by its raw path) and asks for its diff, once per opening: a diff that
  // came is kept, one that failed is asked for again. Before the listing is in, the diff waits
  // for it.
  const toggle = useCallback(
    (raw: string) => {
      const next = new Set(expandedRef.current);
      const diff = diffsRef.current.get(raw);
      if (next.delete(raw)) {
        if (diff?.state === "failed") setDiff(raw, null);
        setExpanded(next);
        return;
      }
      next.add(raw);
      setExpanded(next);
      if (fresh.current && (diff === undefined || diff.state === "failed")) askDiff(raw);
      else if (diff?.state === "failed") setDiff(raw, null);
    },
    [askDiff, setDiff, setExpanded],
  );

  const back = useCallback(() => {
    if (router.canGoBack()) router.back();
    else router.replace("/");
  }, [router]);

  const files = listing.frame?.files ?? NO_FILES;
  const more = listing.frame?.more ?? 0;
  // With the listing failed, nothing is fresh: a diff not in waits for the next listing.
  const stuck = listing.failed !== null;
  const items = useMemo(() => {
    const out: Item[] = [];
    for (const f of files) {
      const key = f.raw;
      const open = expanded.has(f.raw);
      out.push({ kind: "file", key, file: f, open });
      if (!open) continue;
      const diff = diffs.get(f.raw);
      if (diff === undefined && stuck) out.push({ kind: "note", key: `${key}\nask`, text: ASK_AGAIN, words: false });
      else if (diff === undefined || diff.state === "loading") out.push({ kind: "note", key: `${key}\nloading`, text: LOADING_DIFF, words: false });
      else if (diff.state === "failed") out.push({ kind: "note", key: `${key}\nfailed`, text: diff.text, words: true });
      else if (diff.binary) out.push({ kind: "note", key: `${key}\nbinary`, text: BINARY_NOTE, words: false });
      else {
        diff.lines.forEach((line, n) => out.push({ kind: "line", key: `${key}\n${n}`, line }));
        if (diff.clipped || diff.truncated) out.push({ kind: "note", key: `${key}\nclipped`, text: CLIPPED_NOTE, words: false });
      }
    }
    return out;
  }, [files, expanded, diffs, stuck]);

  // While the Mac can't be reached the screen shows its last state, greyed, and takes no tap.
  const offline = hub !== "ready";
  const renderItem = useCallback<ListRenderItem<Item>>(
    ({ item }) =>
      item.kind === "file" ? (
        <FileRow file={item.file} open={item.open} off={offline} onToggle={toggle} />
      ) : item.kind === "line" ? (
        <DiffLineRow line={item.line} />
      ) : (
        <DiffNote text={item.text} words={item.words} />
      ),
    [offline, toggle],
  );

  const s = useMemo(
    () =>
      StyleSheet.create({
        screen: { flex: 1, backgroundColor: theme.bg },
        bar: { height: 48, flexDirection: "row", alignItems: "center", paddingHorizontal: spacing.sm },
        title: { flex: 1, alignItems: "center" },
        heading: { ...type.body, fontWeight: "700", color: theme.fg },
        sub: { ...type.label, color: theme.muted },
        spacer: { width: 44 },
        body: { flex: 1 },
        greyed: { opacity: 0.6 },
        list: { paddingTop: spacing.sm, paddingBottom: spacing.lg },
        empty: { ...type.body, color: theme.muted, paddingHorizontal: SIDE, paddingVertical: spacing.lg },
        status: { paddingHorizontal: SIDE, paddingVertical: spacing.md, gap: spacing.sm },
        failure: { ...type.body, color: theme.fg },
        button: {
          minHeight: 44,
          minWidth: 44,
          alignSelf: "flex-start",
          paddingHorizontal: spacing.xl,
          alignItems: "center",
          justifyContent: "center",
          borderRadius: radius.md,
          borderWidth: 1,
          borderColor: theme.accent,
        },
        retry: { ...type.body, color: theme.accent },
        footer: { gap: spacing.xl, paddingHorizontal: SIDE, paddingTop: spacing.lg },
        more: { ...type.label, color: theme.muted },
        actions: { paddingHorizontal: SIDE, paddingTop: spacing.md },
      }),
    [theme],
  );

  const branch = listing.frame ? branchLines(listing.frame, files.length + more) : null;
  const branchName = branch?.name ?? null;
  const branchDetail = branch?.detail ?? "";
  const footer = useMemo(
    () => (
      <View style={s.footer}>
        {more > 0 ? <Text style={s.more}>{`${more} more files on the Mac`}</Text> : null}
        <ChecksCard session={id} />
        {branchName !== null ? <BranchCard name={branchName} detail={branchDetail} /> : null}
      </View>
    ),
    [s, more, id, branchName, branchDetail],
  );
  const empty = useMemo(
    () =>
      listing.state === "norepo" ? (
        <Text style={s.empty}>{NOT_A_REPOSITORY}</Text>
      ) : listing.state === "ready" ? (
        <Text style={s.empty}>{NO_CHANGES}</Text>
      ) : listing.failed === null && !offline ? (
        <Text style={s.empty}>{LOADING}</Text>
      ) : null,
    [listing.state, listing.failed, offline, s],
  );

  return (
    <View style={[s.screen, { paddingTop: insets.top }]}>
      <View style={s.bar}>
        <Button label="‹" accessibilityLabel="Back" variant="bare" onPress={back} />
        <View style={s.title}>
          <Text accessibilityRole="header" style={s.heading}>
            Changes
          </Text>
          <Text numberOfLines={1} style={s.sub}>
            {listing.frame ? `${name} · ${changeSummary(files, more)}` : name}
          </Text>
        </View>
        <View style={s.spacer} />
      </View>
      <HubBanner state={hub} announce={focused} />
      <View testID="changes-body" style={offline ? [s.body, s.greyed] : s.body}>
        {listing.failed ? (
          <View accessibilityLiveRegion="polite" style={s.status}>
            <Text style={s.failure}>{listing.failed.text}</Text>
            <Pressable accessibilityRole="button" accessibilityLabel="Try again" disabled={offline} onPress={retry} style={s.button}>
              <Text style={s.retry}>Try again</Text>
            </Pressable>
          </View>
        ) : null}
        <FlashList
          testID="changes-list"
          data={items}
          extraData={offline}
          renderItem={renderItem}
          keyExtractor={keyOf}
          getItemType={typeOf}
          ListEmptyComponent={empty}
          ListFooterComponent={footer}
          refreshing={listing.pulling}
          onRefresh={offline ? undefined : pull}
          contentContainerStyle={s.list}
        />
        {/* Commit and Open PR go here (the next task): the list ends above them. */}
        <View testID="changes-actions" style={[s.actions, { paddingBottom: Math.max(insets.bottom, spacing.xl) }]} />
      </View>
    </View>
  );
}

function cardStyles(theme: Theme) {
  return StyleSheet.create({
    line: { ...type.body, color: theme.fg },
    detail: { ...type.body, color: theme.muted },
    pass: { color: theme.accent },
    fail: { color: theme.amber },
  });
}
const DARK_CARDS = cardStyles(dark);
const LIGHT_CARDS = cardStyles(light);
const cardsOf = (theme: Theme) => (theme === light ? LIGHT_CARDS : DARK_CARDS);

/** Checks: the session's last test run, by the heuristic in src/changes.ts. It reads the
 * session's view itself, so streamed output draws only this card. */
const ChecksCard = memo(function ChecksCard({ session }: { session: string }) {
  const c = cardsOf(useTheme());
  const entries = useEntries(session);
  const checks = useMemo(() => lastChecks(entries), [entries]);
  return (
    <Card title="Checks" testID="checks">
      {checks ? (
        <Text style={c.line}>
          <Text style={checks.passed ? c.pass : c.fail}>{checks.passed ? "✓" : "✗"}</Text>
          {` ${checks.command}`}
        </Text>
      ) : (
        <Text style={c.detail}>{NO_TESTS}</Text>
      )}
    </Card>
  );
});

/** Branch: the branch, and how far ahead of its upstream it is or whether anything is uncommitted. */
const BranchCard = memo(function BranchCard({ name, detail }: { name: string; detail: string }) {
  const c = cardsOf(useTheme());
  return (
    <Card title="Branch" testID="branch">
      <Text style={c.line}>{name}</Text>
      <Text style={c.detail}>{detail}</Text>
    </Card>
  );
});
