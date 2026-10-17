import { memo } from "react";
import Svg, { Path } from "react-native-svg";

import { POSES, type PoseName } from "./poses";

interface MascotProps {
  pose: PoseName;
  /** The width in points; the height follows the pose's aspect ratio. */
  size: number;
}

/**
 * The mini Octet in one of its nine poses: one path per colour. react-native-svg
 * has no `shapeRendering`, so pick sizes that are whole multiples of the 59-unit
 * width where crisp pixels matter.
 */
export const Mascot = memo(function Mascot({ pose, size }: MascotProps) {
  const { viewBox, paths } = POSES[pose];
  const [, , w = 1, h = 1] = viewBox.split(" ").map(Number);
  return (
    <Svg
      width={size}
      height={(size * h) / w}
      viewBox={viewBox}
      accessible
      accessibilityRole="image"
      accessibilityLabel={`Octet, ${pose}`}
    >
      {paths.map((p) => (
        <Path key={p.fill} fill={p.fill} d={p.d} />
      ))}
    </Svg>
  );
});
