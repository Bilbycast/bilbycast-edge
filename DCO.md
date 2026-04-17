# Developer Certificate of Origin

All contributions to this repository must be accompanied by a
Developer Certificate of Origin sign-off. The full text is below
and at https://developercertificate.org/.

To sign off on a commit, add the `-s` flag to `git commit`:

    git commit -s -m "your commit message"

This appends a `Signed-off-by: Your Name <you@example.com>` line
using the identity configured in `git config user.name` and
`git config user.email`.

By signing off, you certify the following:

```
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.


Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

## Why we require DCO

bilbycast is dual-licensed (AGPL-3.0-or-later for open-source users,
commercial licence for OEMs — see [LICENSE.commercial](LICENSE.commercial)).
By signing off on your commits you confirm you have the right to
submit the code under AGPL-3.0-or-later, which in turn lets Softside
Tech continue to offer the commercial licence to customers who need
it.

Commits without a `Signed-off-by` line will be rejected by CI and
cannot be merged.
