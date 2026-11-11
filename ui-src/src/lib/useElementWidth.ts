import { type RefObject, useEffect, useRef, useState } from "react";

const DEFAULT_WIDTH = 640;

/** The rendered width of an element, kept current on resize, so an SVG can draw at 1:1 pixel size. */
export function useElementWidth<T extends HTMLElement>(): readonly [RefObject<T | null>, number] {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(DEFAULT_WIDTH);

  useEffect(() => {
    const element = ref.current;
    if (!element) {
      return;
    }
    const observer = new ResizeObserver(([entry]) => {
      if (entry) {
        setWidth(Math.max(1, Math.round(entry.contentRect.width)));
      }
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  return [ref, width];
}
