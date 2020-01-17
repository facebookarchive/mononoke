  $ . "${TEST_FIXTURES}/library.sh"

# Create a repository
  $ setup_mononoke_config
  $ REPOID=1 FILESTORE=1 FILESTORE_CHUNK_SIZE=10 setup_mononoke_repo_config lfs1

# Start a LFS server for this repository (no upstream)
  $ lfs_log="$TESTTMP/lfs.log"
  $ lfs_uri="$(lfs_server --log "$lfs_log")/lfs1"

# Send some data
  $ yes A 2>/dev/null | head -c 2KiB | hg --config extensions.lfs= debuglfssend "$lfs_uri"
  ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746 2048

# Make sure we can read it back
  $ hg --config extensions.lfs= debuglfsreceive ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746 2048 "$lfs_uri" | sha256sum
  ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746  -

# Send again
  $ yes A 2>/dev/null | head -c 2KiB | hg --config extensions.lfs= debuglfssend "$lfs_uri"
  ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746 2048

# Verify that we only uploaded once
  $ cat "$lfs_log"
  POST /lfs1/objects/batch 200 OK
  PUT /lfs1/upload/ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746/2048 200 OK
  POST /lfs1/objects/batch 200 OK
  GET /lfs1/download/d28548bc21aabf04d143886d717d72375e3deecd0dafb3d110676b70a192cb5d 200 OK
  POST /lfs1/objects/batch 200 OK
