# sch_lab table references bpnode headers
target_include_directories(sch_lab.table INTERFACE
    $<TARGET_PROPERTY:bpnode,INCLUDE_DIRECTORIES>
)

install(
    FILES ${MISSION_DEFS}/cfe_es_startup.scr
    DESTINATION ${TGTNAME}/${INSTALL_SUBDIR}
)
