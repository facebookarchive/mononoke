  $ . "${TEST_FIXTURES}/library.sh"

setup configuration
  $ setup_common_config
  $ cd $TESTTMP

setup common configuration
  $ cat >> $HGRCPATH <<EOF
  > [ui]
  > ssh="$DUMMYSSH"
  > EOF

setup repo
  $ hg init repo-hg
  $ cd repo-hg
  $ setup_hg_server
  $ hg debugdrawdag <<EOF
  > C
  > |
  > B
  > |
  > A
  > EOF

create master bookmark
  $ hg bookmark master_bookmark -r tip

blobimport, check blobstore puts are blocked
  $ cd ..
  $ rm -rf repo
  $ blobimport repo-hg/.hg repo --readonly-storage | grep 'root cause:'
  * root cause: ReadOnlyPut("*") (glob)
