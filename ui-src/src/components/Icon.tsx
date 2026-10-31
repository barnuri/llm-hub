const ICON_PATHS = {
  overview: "M3 3h7v7H3zM14 3h7v4h-7zM14 11h7v10h-7zM3 14h7v7H3z",
  insights: "M12 3l2.6 5.6L20.5 9l-4.4 4.1 1.1 6L12 16.2 6.8 19.1l1.1-6L3.5 9l5.9-.4z",
  errors: "M12 3l9.5 17h-19zM12 10v4.5M12 17.5v.01",
  stats: "M4 20V10M10 20V4M16 20v-7M22 20H2",
  auto: "M12 3v3M12 18v3M3 12h3M18 12h3M6 6l2 2M16 16l2 2M6 18l2-2M16 8l2-2M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6z",
  models: "M12 3l8 4.5v9L12 21l-8-4.5v-9zM12 12l8-4.5M12 12v9M12 12L4 7.5",
  profiles: "M4 6h16M4 12h16M4 18h16M8 4v4M16 10v4M10 16v4",
  keys: "M14.5 9.5a4.5 4.5 0 1 1-9 0 4.5 4.5 0 0 1 9 0zM13.2 12.7L21 20.5M17.5 17l2-2",
  setup: "M5 7l5 5-5 5M12 18h7",
  restart: "M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7",
  sun: "M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4",
  moon: "M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z",
  logo: "M12 2.5l8.2 4.75v9.5L12 21.5l-8.2-4.75v-9.5zM12 8l3.5 2v4L12 16l-3.5-2v-4z",
} as const;

export type IconName = keyof typeof ICON_PATHS;

interface IconProps {
  readonly name: IconName;
  readonly className?: string;
}

export function Icon({ name, className }: IconProps) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      width="16"
      height="16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d={ICON_PATHS[name]} />
    </svg>
  );
}
