# sch_lab table references bpnode_msgids.h — need bpnode include dirs
target_include_directories(sch_lab.table INTERFACE
    $<TARGET_PROPERTY:bpnode,INCLUDE_DIRECTORIES>
)
