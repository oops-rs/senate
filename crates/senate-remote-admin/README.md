# senate-remote-admin

Administrator helper and shared authorization format for Senate's experimental
remote machine access. The helper maintains the root-owned listener policy and
device grants. The bridge reads that policy and uses the same enrollment digest
as the helper.

Build the release artifact with a matching Senate checkout:

```sh
./deploy/build-remote-admin.sh <artifact-directory>
```

Install the verified artifact using the staging procedure in the
[Remote machines guide](https://github.com/oops-rs/senate#remote-machines).
The guide covers checksum verification, listener activation, pairing,
revocation and removal. Remote access requires a separate experimental opt-in.
