// The brand of the gate pages, and the marks of the account menus.
import { useState } from "react";
import type { User } from "../api/types";
import { displayName, initials } from "../model/access";

export function Brand() {
  return (
    <div className="brand">
      <span className="brand-mark" aria-hidden="true">
        AP
      </span>
      <span className="brand-name">Rewind</span>
    </div>
  );
}

/** GitHub's mark, drawn inline: nothing is loaded from another origin. */
export function GithubMark({ size = 14 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" fill="currentColor">
      <path d="M12 .3a12 12 0 0 0-3.8 23.38c.6.12.83-.26.83-.57L9 21.07c-3.34.72-4.04-1.61-4.04-1.61-.55-1.39-1.34-1.76-1.34-1.76-1.08-.74.09-.73.09-.73 1.2.09 1.84 1.24 1.84 1.24 1.07 1.83 2.8 1.3 3.49 1 .1-.78.42-1.31.76-1.61-2.67-.3-5.47-1.33-5.47-5.93 0-1.31.47-2.38 1.24-3.22-.14-.3-.54-1.52.1-3.18 0 0 1-.32 3.3 1.23a11.5 11.5 0 0 1 6 0c2.28-1.55 3.29-1.23 3.29-1.23.64 1.66.24 2.88.12 3.18a4.65 4.65 0 0 1 1.23 3.22c0 4.61-2.8 5.63-5.48 5.92.42.36.81 1.1.81 2.22l-.01 3.29c0 .31.2.69.82.57A12 12 0 0 0 12 .3" />
    </svg>
  );
}

/**
 * The member's picture, as the identity provider gives it, or their initials. The picture is the
 * person's own (GitHub's avatar): it is asked for without a referrer, and the initials stand in
 * when it cannot be loaded.
 */
export function Avatar({ user, size = 22 }: { user: User; size?: number }) {
  const [failed, setFailed] = useState(false);
  const style = { width: size, height: size, fontSize: Math.round(size * 0.42) };
  return (
    <span className="avatar" style={style} title={displayName(user)} aria-hidden="true">
      {user.pictureUrl && !failed ? (
        <img
          src={user.pictureUrl}
          alt=""
          referrerPolicy="no-referrer"
          onError={() => setFailed(true)}
        />
      ) : (
        initials(user)
      )}
    </span>
  );
}
