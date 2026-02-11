interface BeakrLogoProps {
  size?: number;
}

export default function BeakrLogo({ size = 24 }: BeakrLogoProps) {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 141 157"
      width={size}
      height={size}
    >
      <defs>
        <linearGradient
          id="beakr-grad"
          x1="9.72"
          y1="123.69"
          x2="92.24"
          y2="35.7"
          gradientUnits="userSpaceOnUse"
        >
          <stop offset="0" stopColor="#7fd3fa" />
          <stop offset="1" stopColor="#3271f5" />
        </linearGradient>
      </defs>
      <path
        fill="url(#beakr-grad)"
        d="M94.48,72.2v-26.76c0-3.37-2.74-6.11-6.11-6.11h-34.16c-3.31,0-6-2.68-6-6V6c0-3.31-2.68-6-6-6H6.28C2.97,0,.29,2.68.29,6v27.92c0,3.31,2.68,6,6,6h35.22c3.31,0,6,2.68,6,6v26.05c0,3.31-2.68,6-6,6H6c-3.31,0-6,2.68-6,6v28.03c0,3.31,2.68,6,6,6h34.7c3.31,0,6,2.68,6,6v26.88c0,3.31,2.68,6,6,6h36.06c3.31,0,6-2.68,6-6v-27.57c0-3.31,2.68-6,6-6h34.43c3.31,0,6-2.68,6-6v-27.09c0-3.31-2.68-6-6-6h-34.7c-3.31,0-6-2.68-6-6ZM48.05,110.83v-26.42c0-3.31,2.68-6,6-6h34.15c3.31,0,6,2.68,6,6v26.42c0,3.31-2.68,6-6,6h-34.15c-3.31,0-6-2.68-6-6Z"
      />
    </svg>
  );
}
