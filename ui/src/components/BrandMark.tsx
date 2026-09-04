type BrandMarkProps = {
  compact?: boolean
  animated?: boolean
}

export function BrandMark({ compact = false, animated = false }: BrandMarkProps) {
  return <span className={`brand-lockup${compact ? ' is-compact' : ''}`} aria-label="selu">
    <svg className={`brand-face${animated ? ' is-awake' : ''}`} viewBox="0 0 64 64" aria-hidden="true">
      <defs>
        <linearGradient id="selu-face" x1="10" y1="8" x2="55" y2="58" gradientUnits="userSpaceOnUse">
          <stop stopColor="#ff7eb3" />
          <stop offset="1" stopColor="#ff6b5a" />
        </linearGradient>
      </defs>
      <circle cx="21" cy="14" r="8" fill="url(#selu-face)" />
      <circle cx="43" cy="14" r="8" fill="url(#selu-face)" />
      <circle cx="32" cy="35" r="24" fill="url(#selu-face)" />
      <ellipse cx="24" cy="31" rx="4.6" ry="5.6" fill="white" />
      <ellipse cx="40" cy="31" rx="4.6" ry="5.6" fill="white" />
      <circle cx="25.2" cy="29.8" r="2.6" fill="#2d2b55" />
      <circle cx="41.2" cy="29.8" r="2.6" fill="#2d2b55" />
      <circle cx="26.2" cy="28.6" r=".9" fill="white" />
      <circle cx="42.2" cy="28.6" r=".9" fill="white" />
      <ellipse cx="18" cy="39" rx="4" ry="2" fill="#ff4070" opacity=".2" />
      <ellipse cx="46" cy="39" rx="4" ry="2" fill="#ff4070" opacity=".2" />
      <path d="M27 42.5 Q32 47 37 42.5" fill="none" stroke="#2d2b55" strokeWidth="1.8" strokeLinecap="round" />
    </svg>
    {!compact && <span className="brand-word">selu<span>.</span></span>}
  </span>
}
