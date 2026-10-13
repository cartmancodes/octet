// Sticking a transcript to its bottom while output streams, unless the user scrolled up.
//
// The list follows the output until the user drags it away from the bottom; while it follows,
// every growth of the content (a new row, or a streaming row getting taller, reported by the
// scroll view's onContentSizeChange after layout) scrolls to the end without animation. Scrolled
// up, nothing moves. Only new output shows the "New output below" pill: the list's last entry
// changed (a new one, or the streaming one grew). Content that merely got taller (a group
// opened, FlashList replacing estimated sizes with measured ones) does not. The pill, or the
// user scrolling back down, follows again. Only a drag or a fling can stop following, so the
// list's own scrolling (to the end, or keeping the visible rows in place as old ones are dropped
// above) never does.
import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import type { NativeScrollEvent, NativeSyntheticEvent } from "react-native";

/** Within this many points of the bottom counts as at the bottom. */
export const NEAR_BOTTOM = 64;

interface Scrollable {
  scrollToEnd(params?: { animated?: boolean }): unknown;
}

export interface Follow {
  /** New output arrived while scrolled up. */
  below: boolean;
  /** Scrolls to the end and follows again (the pill). */
  jump: () => void;
  onScroll: (e: NativeSyntheticEvent<NativeScrollEvent>) => void;
  onScrollBeginDrag: () => void;
  onScrollEndDrag: () => void;
  onMomentumScrollBegin: () => void;
  onMomentumScrollEnd: () => void;
  onContentSizeChange: (width: number, height: number) => void;
}

/** `last` is the list's last entry: a different one while scrolled up is new output, and shows
 * the pill. */
export function useFollow(list: RefObject<Scrollable | null>, last: unknown): Follow {
  const following = useRef(true);
  const touching = useRef(false);
  /** The pill's animated scroll is on its way down: a fling Android reports for it is not the user's. */
  const jumping = useRef(false);
  const height = useRef(0);
  const [below, setBelow] = useState(false);

  const toEnd = useCallback(
    (animated: boolean) => {
      void list.current?.scrollToEnd({ animated });
    },
    [list],
  );

  const onScroll = useCallback((e: NativeSyntheticEvent<NativeScrollEvent>) => {
    const { contentOffset, contentSize, layoutMeasurement } = e.nativeEvent;
    const gap = contentSize.height - (contentOffset.y + layoutMeasurement.height);
    if (gap <= NEAR_BOTTOM) {
      following.current = true;
      jumping.current = false;
      setBelow(false);
    } else if (touching.current && !jumping.current) {
      following.current = false;
    }
  }, []);

  const onScrollBeginDrag = useCallback(() => {
    touching.current = true;
    jumping.current = false; // a finger on the list is the user's, whatever was scrolling
  }, []);
  const onMomentumScrollBegin = useCallback(() => {
    touching.current = true;
  }, []);
  const onScrollEndDrag = useCallback(() => {
    touching.current = false;
  }, []);

  const onContentSizeChange = useCallback(
    (_width: number, h: number) => {
      const grew = h > height.current;
      height.current = h;
      if (grew && following.current) toEnd(false);
    },
    [toEnd],
  );

  // New output while scrolled up: the pill. (The first render is not news.)
  const first = useRef(true);
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    if (!following.current) setBelow(true);
  }, [last]);

  const jump = useCallback(() => {
    following.current = true;
    jumping.current = true;
    setBelow(false);
    toEnd(true);
  }, [toEnd]);

  return {
    below,
    jump,
    onScroll,
    onScrollBeginDrag,
    onScrollEndDrag,
    // A fling is the user's too.
    onMomentumScrollBegin,
    onMomentumScrollEnd: onScrollEndDrag,
    onContentSizeChange,
  };
}
